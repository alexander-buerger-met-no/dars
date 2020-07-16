//! # Data Attribute Structure
//!
//! DAS responses contain additional information about each variable like _fill value_ or history
//! fields.
//!
//! DAS responses are static once constructed from a source.
use std::fmt;

/// DAS (Data Attribute Structure)
pub struct Das(pub String);

#[derive(Debug)]
pub struct Attribute {
    pub name: String,
    pub value: AttrValue,
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    Str(String),
    Float(f32),
    Floats(Vec<f32>),
    Double(f64),
    Doubles(Vec<f64>),
    Short(i16),
    Shorts(Vec<i16>),
    Int(i32),
    Ints(Vec<i32>),
    Uchar(u8),
    Unimplemented(String),
    Ignored(String),
}

/// File type handlers or readers should implement this trait so that a DAS structure can be built.
pub trait ToDas {
    /// Whether dataset has global attributes.
    fn has_global_attributes(&self) -> bool;

    /// Global attributes in dataset.
    fn global_attributes(&self) -> Box<dyn Iterator<Item = Attribute>>;

    /// Variables in dataset.
    fn variables(&self) -> Box<dyn Iterator<Item = String>>;

    /// Attributes for variable in dataset.
    fn variable_attributes(&self, variable: &str) -> Box<dyn Iterator<Item = Attribute>>;
}

const INDENT: usize = 4;

impl<T> From<T> for Das
where
    T: ToDas,
{
    fn from(dataset: T) -> Self {
        let mut das: String = "Attributes {\n".to_string();

        if dataset.has_global_attributes() {
            das.push_str(&format!("{}NC_GLOBAL {{\n", " ".repeat(INDENT)));
            das.push_str(
                &dataset
                    .global_attributes()
                    .map(|a| format!("{}{}\n", " ".repeat(INDENT), Das::format_attr(a)))
                    .collect::<String>(),
            );
            das.push_str(&format!("{}}}\n", " ".repeat(INDENT)));
        }

        for var in dataset.variables() {
            das.push_str(&format!("    {} {{\n", var));
            das.push_str(
                &dataset
                    .variable_attributes(&var)
                    .map(|a| format!("{}{}\n", " ".repeat(INDENT), Das::format_attr(a)))
                    .collect::<String>(),
            );
            das.push_str("    }\n");
        }
        das.push_str("}");

        Das(das)
    }
}

impl fmt::Display for Das {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Das {
    fn format_attr(a: Attribute) -> String {
        use AttrValue::*;

        match a.value {
            Str(s) => format!(
                "{}String {} \"{}\";",
                " ".repeat(INDENT),
                a.name,
                s.escape_default()
            ),
            Float(f) => format!("{}Float32 {} {:+E};", " ".repeat(INDENT), a.name, f),
            Floats(f) => format!(
                "{}Float32 {} {};",
                " ".repeat(INDENT),
                a.name,
                f.iter()
                    .map(|f| format!("{:+E}", f))
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
            Double(f) => format!("{}Float64 {} {:+E};", " ".repeat(INDENT), a.name, f),
            Doubles(f) => format!(
                "{}Float64 {} {};",
                " ".repeat(INDENT),
                a.name,
                f.iter()
                    .map(|f| format!("{:+E}", f))
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
            Short(f) => format!("{}Int16 {} {};", " ".repeat(INDENT), a.name, f),
            Int(f) => format!("{}Int32 {} {};", " ".repeat(INDENT), a.name, f),
            Ints(f) => format!(
                "{}Int32 {} {};",
                " ".repeat(INDENT),
                a.name,
                f.iter()
                    .map(|f| format!("{}", f))
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
            Uchar(n) => format!("{}Byte {} {};", " ".repeat(INDENT), a.name, n),

            Ignored(n) => {
                debug!("Ignored (hidden) DAS field: {:?}: {:?}", a.name, n);
                "".to_string()
            }

            Unimplemented(v) => {
                debug!("Unimplemented attribute: {:?}: {:?}", a.name, v);
                "".to_string()
            }

            v => {
                debug!("Unimplemented DAS field: {:?}: {:?}", a.name, v);
                "".to_string()
            }
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
