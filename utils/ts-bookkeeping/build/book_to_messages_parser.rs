use std::default::Default;
use std::ops::Deref;

use heck::*;
use t4rust_derive::Template;
use tsproto_structs::book_to_messages::*;
use tsproto_structs::messages::Field;
use tsproto_structs::*;

use crate::message_parser::{single_value_serializer, vector_value_serializer};

#[derive(Template)]
#[TemplatePath = "build/BookToMessages.tt"]
#[derive(Debug)]
pub struct BookToMessagesDeclarations<'a>(
	&'a book_to_messages::BookToMessagesDeclarations<'a>,
);

impl<'a> Deref for BookToMessagesDeclarations<'a> {
	type Target = book_to_messages::BookToMessagesDeclarations<'a>;
	fn deref(&self) -> &Self::Target { &self.0 }
}

impl Default for BookToMessagesDeclarations<'static> {
	fn default() -> Self { BookToMessagesDeclarations(&DATA) }
}

fn to_ref_type(s: &str) -> String {
	if s == "String" {
		"&str".into()
	} else {
		s.into()
	}
}

/// The prefix is written before from, if from is a mapped argument
fn arg_to_value(r: &RuleKind, prefix: &str) -> String {
	match r {
		RuleKind::Map { to, .. } | RuleKind::ArgumentMap { to, .. } => {
			let fr = r.from_name().to_snake_case();
			if let RuleKind::Map { .. } = r {
				format!("args.push((\"{}\", {{ {} }}));", to.ts, generate_serializer(to, &format!("{}{}", prefix, fr)))
			} else {
				format!("args.push((\"{}\", {{ {} }}));", to.ts, generate_serializer(to, &fr))
			}
		}
		RuleKind::Function { name, .. } => {
			format!("self.{}(&mut args);", name.to_snake_case())
		}
		RuleKind::ArgumentFunction { from, name, .. } => {
			format!("self.{}(&mut args, {});", name.to_snake_case(), from.to_snake_case())
		}
	}
}

fn get_arguments(r: &RuleKind) -> String {
	match r {
		RuleKind::Map { .. } | RuleKind::Function { .. } => format!(
			"{}: {}",
			r.from_name().to_snake_case(),
			to_ref_type(&r.from().get_rust_type())
		),
		RuleKind::ArgumentMap { from, to } => format!(
			"{}: {}",
			from.to_snake_case(),
			convert_type(&to.type_s, true)
		),
		RuleKind::ArgumentFunction { from, type_s, .. } => {
			format!("{}: {}", from.to_snake_case(), convert_type(type_s, true))
		}
	}
}

pub fn generate_serializer(field: &Field, name: &str) -> String {
	let rust_type = field.get_rust_type("", false);
	if rust_type.starts_with("Vec<") {
		let inner_type = &rust_type[4..rust_type.len()-1];
		vector_value_serializer(field, inner_type, name)
	} else {
		single_value_serializer(field, &rust_type, name)
	}
}
