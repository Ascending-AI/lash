//! Kernel signature and type grammar, including record projection modes.

use super::{Parsed, Parser, Token};
use lash_vocabulary::types::{MapType, Param, RecordType, RecordTypeField, Signature, Type};

impl Parser {
    pub(super) fn signature(&mut self) -> Parsed<Signature> {
        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        while !self.eat(&Token::RParen) {
            let name = self.name()?;
            let optional = self.eat(&Token::Question);
            self.expect(&Token::Colon)?;
            params.push(Param {
                name,
                ty: self.ty()?,
                optional,
            });
            if !self.eat(&Token::Comma) {
                self.expect(&Token::RParen)?;
                break;
            }
        }
        self.expect(&Token::Arrow)?;
        Ok(Signature {
            params,
            result: self.ty()?,
        })
    }

    fn types(&mut self) -> Parsed<Vec<Type>> {
        self.expect(&Token::LParen)?;
        let mut types = Vec::new();
        while !self.eat(&Token::RParen) {
            types.push(self.ty()?);
            if !self.eat(&Token::Comma) {
                self.expect(&Token::RParen)?;
                break;
            }
        }
        Ok(types)
    }

    fn one_type(&mut self) -> Parsed<Type> {
        self.expect(&Token::LParen)?;
        let ty = self.ty()?;
        self.expect(&Token::RParen)?;
        Ok(ty)
    }

    pub(super) fn ty(&mut self) -> Parsed<Type> {
        self.nested(|parser| {
            let word = parser.keyword("a type")?;
            Ok(match word.as_str() {
                "Any" => Type::Any,
                "Json" => Type::Json,
                "Null" => Type::Null,
                "Absent" => Type::Absent,
                "Bool" => Type::Bool,
                "Int" => Type::Int,
                "Float" => Type::Float,
                "Number" => Type::Number,
                "Text" => Type::Text,
                "Bytes" => Type::Bytes,
                "Timestamp" => Type::Timestamp,
                "Error" => Type::Error,
                "Tuple" => Type::Tuple(parser.types()?),
                "Union" => Type::Union(parser.types()?),
                "List" => Type::List(Box::new(parser.one_type()?)),
                "Set" => Type::Set(Box::new(parser.one_type()?)),
                "Task" => Type::Task(Box::new(parser.one_type()?)),
                "Map" => {
                    parser.expect(&Token::LParen)?;
                    let key = parser.ty()?;
                    parser.expect(&Token::Comma)?;
                    let value = parser.ty()?;
                    parser.expect(&Token::RParen)?;
                    Type::Map(Box::new(MapType { key, value }))
                }
                "Handle" => {
                    parser.expect(&Token::LParen)?;
                    let kind = parser.text()?;
                    parser.expect(&Token::RParen)?;
                    Type::Handle(kind)
                }
                "Enum" => {
                    parser.expect(&Token::LParen)?;
                    let mut members = Vec::new();
                    while !parser.eat(&Token::RParen) {
                        members.push(parser.text()?);
                        if !parser.eat(&Token::Comma) {
                            parser.expect(&Token::RParen)?;
                            break;
                        }
                    }
                    Type::Enum(members)
                }
                "Fn" => Type::Function(Box::new(parser.signature()?)),
                "Record" => {
                    parser.expect(&Token::LBrace)?;
                    let mut record = RecordType {
                        fields: Vec::new(),
                        rest: lash_vocabulary::RecordRest::Closed,
                    };
                    while !parser.eat(&Token::RBrace) {
                        if parser.eat(&Token::DotDot) {
                            record.rest = if parser.eat_word("drop") {
                                lash_vocabulary::RecordRest::Drop
                            } else {
                                lash_vocabulary::RecordRest::Keep(Box::new(parser.ty()?))
                            };
                            parser.expect(&Token::RBrace)?;
                            break;
                        }
                        let name = parser.field_name()?;
                        let optional = parser.eat(&Token::Question);
                        parser.expect(&Token::Colon)?;
                        record.fields.push(RecordTypeField {
                            name,
                            ty: parser.ty()?,
                            optional,
                        });
                        if !parser.eat(&Token::Comma) {
                            parser.expect(&Token::RBrace)?;
                            break;
                        }
                    }
                    Type::Record(record)
                }
                _ => return Err(parser.unexpected_previous("a type")),
            })
        })
    }
}
