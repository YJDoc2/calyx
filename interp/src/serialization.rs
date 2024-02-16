use fraction::Fraction;
use itertools::Itertools;
use serde::Serialize;
use std::fmt::{Debug, Display};

use crate::{
    primitives::Primitive, structures::state_views::FullySerialize,
    utils::PrintCode, values::Value,
};

/// An enum wrapping over a tuple representing the shape of a multi-dimensional
/// array
#[derive(Clone)]
pub enum Shape {
    D1((usize,)),
    D2((usize, usize)),
    D3((usize, usize, usize)),
    D4((usize, usize, usize, usize)),
}

impl Shape {
    fn is_1d(&self) -> bool {
        matches!(self, Shape::D1(_))
    }

    pub(crate) fn dim_str(&self) -> String {
        match self {
            Shape::D1(_) => String::from("1D"),
            Shape::D2(_) => String::from("2D"),
            Shape::D3(_) => String::from("3D"),
            Shape::D4(_) => String::from("4D"),
        }
    }
}
impl From<usize> for Shape {
    fn from(u: usize) -> Self {
        Shape::D1((u,))
    }
}
impl From<(usize,)> for Shape {
    fn from(u: (usize,)) -> Self {
        Shape::D1(u)
    }
}
impl From<(usize, usize)> for Shape {
    fn from(u: (usize, usize)) -> Self {
        Shape::D2(u)
    }
}

impl From<(usize, usize, usize)> for Shape {
    fn from(u: (usize, usize, usize)) -> Self {
        Shape::D3(u)
    }
}

impl From<(usize, usize, usize, usize)> for Shape {
    fn from(u: (usize, usize, usize, usize)) -> Self {
        Shape::D4(u)
    }
}

/// A wrapper enum used during serialization. It represents either an unsigned integer,
/// or a signed integer and is serialized as the underlying integer. This also allows
/// mixed serialization of signed and unsigned values
#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum Entry {
    U(u64),
    I(i64),
    Frac(Fraction),
    Value(Value),
}

impl From<u64> for Entry {
    fn from(u: u64) -> Self {
        Self::U(u)
    }
}

impl From<i64> for Entry {
    fn from(i: i64) -> Self {
        Self::I(i)
    }
}

impl From<Fraction> for Entry {
    fn from(f: Fraction) -> Self {
        Self::Frac(f)
    }
}

impl Entry {
    pub fn from_val_code(val: &Value, code: &PrintCode) -> Self {
        match code {
            PrintCode::Unsigned => val.as_u64().into(),
            PrintCode::Signed => val.as_i64().into(),
            PrintCode::UFixed(f) => val.as_ufp(*f).into(),
            PrintCode::SFixed(f) => val.as_sfp(*f).into(),
            PrintCode::Binary => Entry::Value(val.clone()),
        }
    }
}

impl Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Entry::U(v) => write!(f, "{}", v),
            Entry::I(v) => write!(f, "{}", v),
            Entry::Frac(v) => write!(f, "{}", v),
            Entry::Value(v) => write!(f, "{}", v),
        }
    }
}

impl Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

#[derive(Clone)]
pub enum Serializable {
    Empty,
    Val(Entry),
    Array(Vec<Entry>, Shape),
    Full(FullySerialize),
}

impl Serializable {
    pub fn has_state(&self) -> bool {
        !matches!(self, Serializable::Empty)
    }
}

impl Display for Serializable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Serializable::Empty => write!(f, ""),
            Serializable::Val(v) => write!(f, "{}", v),
            Serializable::Array(arr, shape) => {
                write!(f, "{}", format_array(arr, shape))
            }
            full @ Serializable::Full(_) => {
                write!(f, "{}", serde_json::to_string(full).unwrap())
            }
        }
    }
}

impl Serialize for Serializable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Serializable::Empty => serializer.serialize_unit(),
            Serializable::Val(u) => u.serialize(serializer),
            Serializable::Array(arr, shape) => {
                let arr: Vec<&Entry> = arr.iter().collect();
                if shape.is_1d() {
                    return arr.serialize(serializer);
                }
                // there's probably a better way to write this
                match shape {
                    Shape::D2(shape) => {
                        let mem = arr
                            .iter()
                            .chunks(shape.1)
                            .into_iter()
                            .map(|x| x.into_iter().collect::<Vec<_>>())
                            .collect::<Vec<_>>();
                        mem.serialize(serializer)
                    }
                    Shape::D3(shape) => {
                        let mem = arr
                            .iter()
                            .chunks(shape.1 * shape.2)
                            .into_iter()
                            .map(|x| {
                                x.into_iter()
                                    .chunks(shape.2)
                                    .into_iter()
                                    .map(|y| y.into_iter().collect::<Vec<_>>())
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>();
                        mem.serialize(serializer)
                    }
                    Shape::D4(shape) => {
                        let mem = arr
                            .iter()
                            .chunks(shape.2 * shape.1 * shape.3)
                            .into_iter()
                            .map(|x| {
                                x.into_iter()
                                    .chunks(shape.2 * shape.3)
                                    .into_iter()
                                    .map(|y| {
                                        y.into_iter()
                                            .chunks(shape.3)
                                            .into_iter()
                                            .map(|z| {
                                                z.into_iter()
                                                    .collect::<Vec<_>>()
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>();
                        mem.serialize(serializer)
                    }
                    Shape::D1(_) => unreachable!(),
                }
            }
            Serializable::Full(s) => s.serialize(serializer),
        }
    }
}

impl Serialize for dyn Primitive {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.serialize(None).serialize(serializer)
    }
}

fn format_array(arr: &[Entry], shape: &Shape) -> String {
    match shape {
        Shape::D2(shape) => {
            let mem = arr
                .iter()
                .chunks(shape.1)
                .into_iter()
                .map(|x| x.into_iter().collect::<Vec<_>>())
                .collect::<Vec<_>>();
            format!("{:?}", mem)
        }
        Shape::D3(shape) => {
            let mem = arr
                .iter()
                .chunks(shape.1 * shape.0)
                .into_iter()
                .map(|x| {
                    x.into_iter()
                        .chunks(shape.2)
                        .into_iter()
                        .map(|y| y.into_iter().collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            format!("{:?}", mem)
        }
        Shape::D4(shape) => {
            let mem = arr
                .iter()
                .chunks(shape.2 * shape.1 * shape.3)
                .into_iter()
                .map(|x| {
                    x.into_iter()
                        .chunks(shape.2 * shape.3)
                        .into_iter()
                        .map(|y| {
                            y.into_iter()
                                .chunks(shape.3)
                                .into_iter()
                                .map(|z| z.into_iter().collect::<Vec<_>>())
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            format!("{:?}", mem)
        }
        Shape::D1(_) => {
            format!("{:?}", arr)
        }
    }
}