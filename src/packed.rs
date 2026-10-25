//! `packed@1` for the simulator. A call's values are untyped JSON here, and
//! `packed@1` puts no tags or widths on the wire, so the schema's types drive
//! the runtime's own [`Packed`] serializer and deserializer: the bytes are the
//! ones a generated peer writes, and decoding refuses what it refuses.
//!
//! A value is shaped as everywhere else in the simulator: a struct or an
//! error is an object of its fields, an enum value its variant's name, a
//! call's arguments an array in declaration order.

use std::fmt;

use comline_runtime::contract::{RuntimeError, WireFormat};
use comline_runtime::format::Packed;
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::ser::{
    Error as _, Serialize, SerializeSeq, SerializeStruct, SerializeTuple, Serializer,
};
use serde_json::{Map, Number, Value};

use crate::shape::{ArgShape, FieldShape, TypeDef, TypeRef};

/// What a body holds, which decides how it's laid out.
#[derive(Clone, Copy, Debug)]
pub enum Body<'a> {
    /// A call's arguments: a JSON array, in declaration order.
    Args(&'a [ArgShape]),
    /// A function's return value; `None` for a one-way call. `()` and `None`
    /// are no bytes at all.
    Return(Option<&'a TypeRef>),
    /// An error's fields: a JSON object.
    Fields(&'a [FieldShape]),
}

/// Encode `value`, laid out as `body`. `types` is every type the project
/// declares, looked up by name.
pub fn encode(
    value: &Value,
    body: Body<'_>,
    types: &[TypeDef],
    out: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    let typed = Typed {
        value,
        ty: Ty::of_body(body),
        types,
    };
    Packed.encode(&typed, out)
}

/// Decode `bytes`, laid out as `body`, into the simulator's JSON shape.
pub fn decode(bytes: &[u8], body: Body<'_>, types: &[TypeDef]) -> Result<Value, RuntimeError> {
    let seed = Seed {
        ty: Ty::of_body(body),
        types,
    };
    Packed.decode_seed(seed, bytes)
}

/// The layout of one value.
#[derive(Clone, Copy)]
enum Ty<'a> {
    Of(&'a TypeRef),
    /// An optional field: absent, or present and then the value.
    Optional(&'a TypeRef),
    Args(&'a [ArgShape]),
    Fields(&'a [FieldShape]),
    Unit,
}

impl<'a> Ty<'a> {
    fn of_body(body: Body<'a>) -> Self {
        match body {
            Body::Args(args) => Ty::Args(args),
            Body::Return(Some(ty)) => Ty::Of(ty),
            Body::Return(None) => Ty::Unit,
            Body::Fields(fields) => Ty::Fields(fields),
        }
    }

    fn of_field(field: &'a FieldShape) -> Self {
        if field.optional {
            Ty::Optional(&field.ty)
        } else {
            Ty::Of(&field.ty)
        }
    }
}

fn find<'a>(types: &'a [TypeDef], name: &str) -> Option<&'a TypeDef> {
    types.iter().find(|t| t.name() == name)
}

fn unknown(name: &str) -> String {
    format!("`packed@1` can't encode a `{name}`: the project declares no such type")
}

// ── encoding ────────────────────────────────────────────────────────────────

struct Typed<'a> {
    value: &'a Value,
    ty: Ty<'a>,
    types: &'a [TypeDef],
}

impl<'a> Typed<'a> {
    fn with(&self, value: &'a Value, ty: Ty<'a>) -> Self {
        Typed {
            value,
            ty,
            types: self.types,
        }
    }

    fn fields<S: Serializer>(&self, fields: &'a [FieldShape], s: S) -> Result<S::Ok, S::Error> {
        let Value::Object(object) = self.value else {
            return Err(S::Error::custom(format!(
                "{} isn't an object of fields",
                self.value
            )));
        };
        let mut out = s.serialize_struct("", fields.len())?;
        for field in fields {
            let value = object.get(&field.name).unwrap_or(&Value::Null);
            out.serialize_field("", &self.with(value, Ty::of_field(field)))?;
        }
        out.end()
    }
}

impl Serialize for Typed<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let value = self.value;
        match self.ty {
            Ty::Unit => s.serialize_unit(),
            Ty::Optional(ty) => match value {
                Value::Null => s.serialize_none(),
                value => s.serialize_some(&self.with(value, Ty::Of(ty))),
            },
            // Arguments come named (`{"text": ..}`, what the playground sends)
            // or in declaration order; either way they go out in order.
            Ty::Args(args) => {
                let mut out = s.serialize_tuple(args.len())?;
                match value {
                    Value::Object(named) => {
                        for arg in args {
                            let item = named.get(&arg.name).unwrap_or(&Value::Null);
                            out.serialize_element(&self.with(item, Ty::Of(&arg.ty)))?;
                        }
                    }
                    Value::Array(items) if items.len() == args.len() => {
                        for (arg, item) in args.iter().zip(items) {
                            out.serialize_element(&self.with(item, Ty::Of(&arg.ty)))?;
                        }
                    }
                    Value::Null if args.is_empty() => {}
                    other => {
                        return Err(S::Error::custom(format!(
                            "arguments {other} don't fit the function's {}",
                            args.len()
                        )))
                    }
                }
                out.end()
            }
            Ty::Fields(fields) => self.fields(fields, s),
            Ty::Of(TypeRef::Unit) => s.serialize_unit(),
            Ty::Of(TypeRef::Prim { name }) => primitive(name, value, s),
            Ty::Of(TypeRef::Ref { name }) => match find(self.types, name) {
                Some(TypeDef::Struct { fields, .. }) => self.fields(fields, s),
                Some(TypeDef::Enum { variants, .. }) => {
                    let index = value
                        .as_str()
                        .and_then(|v| variants.iter().position(|variant| variant == v))
                        .ok_or_else(|| {
                            S::Error::custom(format!("{value} isn't a `{name}` variant"))
                        })?;
                    s.serialize_unit_variant("", index as u32, "")
                }
                Some(TypeDef::Alias { of, .. }) => self.with(value, Ty::Of(of)).serialize(s),
                None => Err(S::Error::custom(unknown(name))),
            },
            Ty::Of(TypeRef::Array { of, len }) => {
                let Value::Array(items) = value else {
                    return Err(S::Error::custom(format!("{value} isn't an array")));
                };
                match len {
                    None => {
                        let mut out = s.serialize_seq(Some(items.len()))?;
                        for item in items {
                            out.serialize_element(&self.with(item, Ty::Of(of)))?;
                        }
                        out.end()
                    }
                    Some(n) => {
                        if items.len() as u64 != *n {
                            return Err(S::Error::custom(format!(
                                "{} elements, where the array holds {n}",
                                items.len()
                            )));
                        }
                        let mut out = s.serialize_tuple(items.len())?;
                        for item in items {
                            out.serialize_element(&self.with(item, Ty::Of(of)))?;
                        }
                        out.end()
                    }
                }
            }
            // The first member the value fits, by its index; the same rule
            // as the TypeScript runtime's union layout.
            Ty::Of(TypeRef::Union { of }) => {
                for (index, member) in of.iter().enumerate() {
                    let typed = self.with(value, Ty::Of(member));
                    if Packed.encode(&typed, &mut Vec::new()).is_ok() {
                        return s.serialize_newtype_variant("", index as u32, "", &typed);
                    }
                }
                Err(S::Error::custom(format!(
                    "{value} fits no member of the union"
                )))
            }
        }
    }
}

fn primitive<S: Serializer>(name: &str, value: &Value, s: S) -> Result<S::Ok, S::Error> {
    let bad = || S::Error::custom(format!("{value} isn't a `{name}`"));
    let unsigned = || value.as_u64().or_else(|| value.as_str()?.parse().ok());
    let signed = || value.as_i64().or_else(|| value.as_str()?.parse().ok());
    match name {
        "bool" => s.serialize_bool(value.as_bool().ok_or_else(bad)?),
        "string" | "str" => s.serialize_str(value.as_str().ok_or_else(bad)?),
        "u8" => s.serialize_u8(unsigned().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "u16" => s.serialize_u16(unsigned().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "u32" => s.serialize_u32(unsigned().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "u64" => s.serialize_u64(unsigned().ok_or_else(bad)?),
        "u128" => s.serialize_u128(
            value
                .as_u64()
                .map(u128::from)
                .or_else(|| value.as_str()?.parse().ok())
                .ok_or_else(bad)?,
        ),
        "s8" => s.serialize_i8(signed().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "s16" => s.serialize_i16(signed().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "s32" => s.serialize_i32(signed().and_then(|n| n.try_into().ok()).ok_or_else(bad)?),
        "s64" => s.serialize_i64(signed().ok_or_else(bad)?),
        "s128" => s.serialize_i128(
            value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_str()?.parse().ok())
                .ok_or_else(bad)?,
        ),
        "f32" => s.serialize_f32(value.as_f64().ok_or_else(bad)? as f32),
        "f64" => s.serialize_f64(value.as_f64().ok_or_else(bad)?),
        other => Err(S::Error::custom(unknown(other))),
    }
}

// ── decoding ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Seed<'a> {
    ty: Ty<'a>,
    types: &'a [TypeDef],
}

impl<'a> Seed<'a> {
    fn with(self, ty: Ty<'a>) -> Self {
        Seed {
            ty,
            types: self.types,
        }
    }
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        match self.ty {
            Ty::Unit | Ty::Of(TypeRef::Unit) => d.deserialize_unit(Scalar),
            Ty::Optional(ty) => d.deserialize_option(Optional(self.with(Ty::Of(ty)))),
            Ty::Args(args) => d.deserialize_tuple(args.len(), Args(self, args)),
            Ty::Fields(fields) => d.deserialize_tuple(fields.len(), Fields(self, fields)),
            Ty::Of(TypeRef::Prim { name }) => match name.as_str() {
                "bool" => d.deserialize_bool(Scalar),
                "string" | "str" => d.deserialize_string(Scalar),
                "u8" => d.deserialize_u8(Scalar),
                "u16" => d.deserialize_u16(Scalar),
                "u32" => d.deserialize_u32(Scalar),
                "u64" => d.deserialize_u64(Scalar),
                "u128" => d.deserialize_u128(Scalar),
                "s8" => d.deserialize_i8(Scalar),
                "s16" => d.deserialize_i16(Scalar),
                "s32" => d.deserialize_i32(Scalar),
                "s64" => d.deserialize_i64(Scalar),
                "s128" => d.deserialize_i128(Scalar),
                "f32" => d.deserialize_f32(Scalar),
                "f64" => d.deserialize_f64(Scalar),
                other => Err(de::Error::custom(unknown(other))),
            },
            Ty::Of(TypeRef::Ref { name }) => match find(self.types, name) {
                Some(TypeDef::Struct { fields, .. }) => {
                    d.deserialize_tuple(fields.len(), Fields(self, fields))
                }
                Some(TypeDef::Enum { variants, .. }) => {
                    d.deserialize_enum("", &[], Variant(variants))
                }
                Some(TypeDef::Alias { of, .. }) => self.with(Ty::Of(of)).deserialize(d),
                None => Err(de::Error::custom(unknown(name))),
            },
            Ty::Of(TypeRef::Array { of, len: None }) => {
                d.deserialize_seq(Elements(self.with(Ty::Of(of))))
            }
            Ty::Of(TypeRef::Array { of, len: Some(n) }) => {
                d.deserialize_tuple(*n as usize, Elements(self.with(Ty::Of(of))))
            }
            Ty::Of(TypeRef::Union { of }) => d.deserialize_enum("", &[], Member(self, of)),
        }
    }
}

/// A primitive, or `()`.
struct Scalar;

impl<'de> Visitor<'de> for Scalar {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a primitive value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    /// Beyond 64 bits, a string: JSON numbers stop there.
    fn visit_u128<E: de::Error>(self, v: u128) -> Result<Value, E> {
        Ok(u64::try_from(v).map_or_else(|_| Value::String(v.to_string()), Value::from))
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<Value, E> {
        Ok(i64::try_from(v).map_or_else(|_| Value::String(v.to_string()), Value::from))
    }

    /// Through its shortest decimal, so `1.1f32` reads back as `1.1` rather
    /// than the `f64` nearest the `f32`.
    fn visit_f32<E: de::Error>(self, v: f32) -> Result<Value, E> {
        let shortest: f64 = v.to_string().parse().unwrap_or(f64::from(v));
        Ok(Number::from_f64(shortest).map_or(Value::Null, Value::Number))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }
}

struct Optional<'a>(Seed<'a>);

impl<'de> Visitor<'de> for Optional<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an optional value")
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.0.deserialize(d)
    }
}

struct Args<'a>(Seed<'a>, &'a [ArgShape]);

impl<'de> Visitor<'de> for Args<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} arguments", self.1.len())
    }

    /// Decoded arguments are named, the shape behaviours and scripts read
    /// (`params.text`) whatever the wire format.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut named = serde_json::Map::with_capacity(self.1.len());
        for (i, arg) in self.1.iter().enumerate() {
            let item = seq
                .next_element_seed(self.0.with(Ty::Of(&arg.ty)))?
                .ok_or_else(|| de::Error::invalid_length(i, &self))?;
            named.insert(arg.name.clone(), item);
        }
        Ok(Value::Object(named))
    }
}

struct Fields<'a>(Seed<'a>, &'a [FieldShape]);

impl<'de> Visitor<'de> for Fields<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} fields", self.1.len())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        for (i, field) in self.1.iter().enumerate() {
            let value = seq
                .next_element_seed(self.0.with(Ty::of_field(field)))?
                .ok_or_else(|| de::Error::invalid_length(i, &self))?;
            object.insert(field.name.clone(), value);
        }
        Ok(Value::Object(object))
    }
}

struct Elements<'a>(Seed<'a>);

impl<'de> Visitor<'de> for Elements<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self.0)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }
}

/// A variant's index on the wire.
struct Index;

impl<'de> DeserializeSeed<'de> for Index {
    type Value = usize;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<usize, D::Error> {
        d.deserialize_u32(Index)
    }
}

impl<'de> Visitor<'de> for Index {
    type Value = usize;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a variant index")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<usize, E> {
        usize::try_from(v).map_err(|_| E::custom("variant index out of range"))
    }
}

/// An enum value: its variant's name.
struct Variant<'a>(&'a [String]);

impl<'de> Visitor<'de> for Variant<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an enum variant")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let (index, variant) = data.variant_seed(Index)?;
        variant.unit_variant()?;
        let name = self
            .0
            .get(index)
            .ok_or_else(|| de::Error::custom(format!("no variant {index}")))?;
        Ok(Value::String(name.clone()))
    }
}

/// A union value: its member's, read by the member's index.
struct Member<'a>(Seed<'a>, &'a [TypeRef]);

impl<'de> Visitor<'de> for Member<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a union member")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let (index, variant) = data.variant_seed(Index)?;
        let member = self
            .1
            .get(index)
            .ok_or_else(|| de::Error::custom(format!("no union member {index}")))?;
        variant.newtype_variant_seed(self.0.with(Ty::Of(member)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use serde_json::json;

    fn prim(name: &str) -> TypeRef {
        TypeRef::Prim { name: name.into() }
    }

    fn named(name: &str) -> TypeRef {
        TypeRef::Ref { name: name.into() }
    }

    fn field(name: &str, ty: TypeRef, optional: bool) -> FieldShape {
        FieldShape {
            name: name.into(),
            ty,
            optional,
        }
    }

    /// The `Player` example from the `packed@1` reference page.
    fn player_types() -> Vec<TypeDef> {
        vec![
            TypeDef::Enum {
                name: "Class".into(),
                variants: vec!["Warrior".into(), "Mage".into()],
            },
            TypeDef::Struct {
                name: "Player".into(),
                fields: vec![
                    field("id", prim("u32"), false),
                    field("hp", prim("u16"), false),
                    field("name", prim("string"), false),
                    field("guild", prim("string"), true),
                    field("class", named("Class"), false),
                    field(
                        "pos",
                        TypeRef::Array {
                            of: Box::new(prim("f32")),
                            len: Some(2),
                        },
                        false,
                    ),
                ],
            },
        ]
    }

    fn player() -> Value {
        json!({ "id": 7, "hp": 100, "name": "ab", "guild": null, "class": "Mage", "pos": [1.0, 2.0] })
    }

    fn encoded(value: &Value, ty: &TypeRef, types: &[TypeDef]) -> Vec<u8> {
        let mut out = Vec::new();
        encode(value, Body::Return(Some(ty)), types, &mut out).unwrap();
        out
    }

    /// The bytes the reference page documents, byte for byte.
    #[test]
    fn the_documented_player_is_19_bytes() {
        let bytes = encoded(&player(), &named("Player"), &player_types());
        assert_eq!(
            bytes,
            [
                0x07, 0, 0, 0, // id
                0x64, 0, // hp
                0x02, b'a', b'b', // name
                0x00, // guild: absent
                0x01, // class: Mage
                0, 0, 0x80, 0x3f, // pos[0]
                0, 0, 0, 0x40, // pos[1]
            ]
        );
        let back = decode(
            &bytes,
            Body::Return(Some(&named("Player"))),
            &player_types(),
        )
        .unwrap();
        assert_eq!(back, player());
    }

    /// The same value through the runtime's `Packed` and a Rust type mirroring
    /// the schema, as a generated peer would encode it.
    #[test]
    fn matches_a_generated_peer_byte_for_byte() {
        #[derive(Serialize)]
        enum Class {
            #[allow(dead_code)]
            Warrior,
            Mage,
        }
        #[derive(Serialize)]
        struct Player {
            id: u32,
            hp: u16,
            name: String,
            guild: Option<String>,
            class: Class,
            pos: [f32; 2],
            tags: Vec<u8>,
            big: u128,
            delta: i16,
        }
        let mut types = player_types();
        let TypeDef::Struct { fields, .. } = &mut types[1] else {
            unreachable!()
        };
        fields[3] = field("guild", prim("string"), true);
        fields.push(field(
            "tags",
            TypeRef::Array {
                of: Box::new(prim("u8")),
                len: None,
            },
            false,
        ));
        fields.push(field("big", prim("u128"), false));
        fields.push(field("delta", prim("s16"), false));

        let rust = Player {
            id: 7,
            hp: 100,
            name: "ab".into(),
            guild: Some("cd".into()),
            class: Class::Mage,
            pos: [1.5, -2.0],
            tags: vec![1, 2, 3],
            big: u128::MAX,
            delta: -300,
        };
        let mut want = Vec::new();
        Packed.encode(&rust, &mut want).unwrap();

        let value = json!({
            "id": 7, "hp": 100, "name": "ab", "guild": "cd", "class": "Mage",
            "pos": [1.5, -2.0], "tags": [1, 2, 3], "big": u128::MAX.to_string(), "delta": -300,
        });
        assert_eq!(encoded(&value, &named("Player"), &types), want);
        assert_eq!(
            decode(&want, Body::Return(Some(&named("Player"))), &types).unwrap(),
            value
        );
    }

    #[test]
    fn arguments_and_errors_are_laid_out_as_tuples_and_structs() {
        let args = [
            ArgShape {
                name: "text".into(),
                ty: prim("string"),
            },
            ArgShape {
                name: "n".into(),
                ty: prim("u32"),
            },
        ];
        let mut out = Vec::new();
        encode(&json!(["hi", 5]), Body::Args(&args), &[], &mut out).unwrap();
        let mut want = Vec::new();
        Packed.encode(&("hi", 5u32), &mut want).unwrap();
        assert_eq!(out, want);
        // Named arguments lay out the same, and decode named.
        let mut named = Vec::new();
        encode(
            &json!({ "n": 5, "text": "hi" }),
            Body::Args(&args),
            &[],
            &mut named,
        )
        .unwrap();
        assert_eq!(named, want);
        assert_eq!(
            decode(&out, Body::Args(&args), &[]).unwrap(),
            json!({ "text": "hi", "n": 5 })
        );
        assert!(encode(&json!(["hi"]), Body::Args(&args), &[], &mut Vec::new()).is_err());

        let fields = [field("reason", prim("string"), false)];
        let mut out = Vec::new();
        encode(
            &json!({ "reason": "no" }),
            Body::Fields(&fields),
            &[],
            &mut out,
        )
        .unwrap();
        assert_eq!(out, [2, b'n', b'o']);
        assert_eq!(
            decode(&out, Body::Fields(&fields), &[]).unwrap(),
            json!({ "reason": "no" })
        );
    }

    #[test]
    fn a_unit_return_and_a_one_way_call_are_no_bytes() {
        for body in [Body::Return(None), Body::Return(Some(&TypeRef::Unit))] {
            let mut out = Vec::new();
            encode(&Value::Null, body, &[], &mut out).unwrap();
            assert!(out.is_empty());
            assert_eq!(decode(&[], body, &[]).unwrap(), Value::Null);
        }
    }

    #[test]
    fn an_alias_is_its_target_on_the_wire() {
        let types = [TypeDef::Alias {
            name: "Handle".into(),
            of: prim("string"),
        }];
        let bytes = encoded(&json!("ada"), &named("Handle"), &types);
        assert_eq!(bytes, [3, b'a', b'd', b'a']);
        assert_eq!(
            decode(&bytes, Body::Return(Some(&named("Handle"))), &types).unwrap(),
            json!("ada")
        );
    }

    #[test]
    fn a_union_value_is_its_members_index_then_the_member() {
        let union = TypeRef::Union {
            of: vec![prim("u32"), prim("string")],
        };
        let bytes = encoded(&json!("x"), &union, &[]);
        assert_eq!(bytes, [1, 1, b'x']);
        assert_eq!(
            decode(&bytes, Body::Return(Some(&union)), &[]).unwrap(),
            json!("x")
        );
        assert_eq!(encoded(&json!(9), &union, &[]), [0, 9, 0, 0, 0]);
    }

    #[test]
    fn what_a_peer_would_refuse_is_refused() {
        let u8_ty = prim("u8");
        let bool_ty = prim("bool");
        let one = |ty| Body::Return(Some(ty));
        assert!(
            decode(&[1, 2], one(&u8_ty), &[]).is_err(),
            "a trailing byte"
        );
        assert!(
            decode(&[2], one(&bool_ty), &[]).is_err(),
            "a bool other than 0/1"
        );
        assert!(decode(&[], one(&u8_ty), &[]).is_err(), "truncated");

        let mut out = Vec::new();
        assert!(
            encode(&json!(300), one(&u8_ty), &[], &mut out).is_err(),
            "out of range"
        );
        let nope = named("Nope");
        assert!(
            encode(&json!("x"), one(&nope), &[], &mut out).is_err(),
            "an unknown type"
        );
        let fixed = TypeRef::Array {
            of: Box::new(u8_ty.clone()),
            len: Some(2),
        };
        assert!(
            encode(&json!([1]), one(&fixed), &[], &mut out).is_err(),
            "wrong length"
        );
    }
}
