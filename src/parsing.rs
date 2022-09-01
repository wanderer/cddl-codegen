use cddl::ast::*;
use either::{Either};
use std::collections::{BTreeMap};

use crate::comment_ast::{RuleMetadata, metadata_from_comments};
use crate::intermediate::{
    AliasIdent,
    CDDLIdent,
    EnumVariant,
    FixedValue,
    GenericDef,
    GenericInstance,
    IntermediateTypes,
    Primitive,
    Representation,
    RustIdent,
    RustRecord,
    RustStruct,
    RustType,
    RustField,
    VariantIdent,
};
use crate::utils::{
    append_number_if_duplicate,
    convert_to_camel_case,
    convert_to_snake_case,
    is_identifier_user_defined,
};

#[derive(Clone, Debug)]
enum ControlOperator {
    Range((Option<isize>, Option<isize>)),
    CBOR(RustType),
}

struct Type2AndParent<'a> {
    type2: &'a Type2<'a>,
    parent: &'a Type1<'a>,
}

pub fn parse_rule(types: &mut IntermediateTypes, cddl_rule: &cddl::ast::Rule) {
    match cddl_rule {
        cddl::ast::Rule::Type{ rule, .. } => {
            // (1) is_type_choice_alternate ignored since shelley.cddl doesn't need it
            //     It's used, but used for no reason as it is the initial definition
            //     (which is also valid cddl), but it would be fine as = instead of /=
            // (2) ignores control operators - only used in shelley spec to limit string length for application metadata
            let rust_ident = RustIdent::new(CDDLIdent::new(rule.name.to_string()));
            let generic_params = rule
                .generic_params
                .as_ref()
                .map(|gp| gp.params.iter().map(|id| RustIdent::new(CDDLIdent::new(id.param.to_string()))).collect::<Vec<_>>());
            if rule.value.type_choices.len() == 1 {
                let choice = &rule.value.type_choices.first().unwrap();
                parse_type(types, &rust_ident, choice, None, generic_params);
            } else {
                parse_type_choices(types, &rust_ident, &rule.value.type_choices, None, generic_params);
            }
        },
        cddl::ast::Rule::Group{ rule, .. } => {
            assert_eq!(rule.generic_params, None, "{}: Generics not supported on plain groups", rule.name);
            // Freely defined group - no need to generate anything outside of group module
            match &rule.entry {
                cddl::ast::GroupEntry::InlineGroup{ .. } => (),// already handled in main.rs
                x => panic!("Group rule with non-inline group? {:?}", x),
            }
        },
    }
}

fn parse_type_choices(types: &mut IntermediateTypes, name: &RustIdent, type_choices: &Vec<TypeChoice>, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let optional_inner_type = if type_choices.len() == 2 {
        let a = &type_choices[0].type1;
        let b = &type_choices[1].type1;
        if type2_is_null(&a.type2) {
            Some(b)
        } else if type2_is_null(&b.type2) {
            Some(a)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(inner_type2) = optional_inner_type {
        if generic_params.is_some() {
            // the current generic support relies on having a RustStruct to swap out the types with
            // but that won't happen with T / null types since we generate an alias instead
            todo!("support foo<T> = T / null");
        }
        let inner_rust_type = rust_type_from_type1(types, inner_type2);
        let final_type = match tag {
            Some(tag) => RustType::Tagged(tag, Box::new(RustType::Optional(Box::new(inner_rust_type)))),
            None => RustType::Optional(Box::new(inner_rust_type)),
        };
        types.register_type_alias(name.clone(), final_type, true, true);
    } else {
        let variants = create_variants_from_type_choices(types, type_choices);
        let rust_struct = RustStruct::new_type_choice(name.clone(), tag, variants);
        match generic_params {
            Some(params) => types.register_generic_def(GenericDef::new(params, rust_struct)),
            None => types.register_rust_struct(rust_struct),
        };
    }
}

fn type2_to_number_literal(type2: &Type2) -> isize {
    match type2 {
        Type2::UintValue{ value, .. } => *value as isize,
        Type2::IntValue{ value, .. } => *value,
        _ => panic!("Value specified: {:?} must be a number literal to be used here", type2),
    }
}

fn parse_control_operator(types: &mut IntermediateTypes, parent: &Type2AndParent, operator: &Operator) -> ControlOperator {
    let lower_bound = match parent.type2 {
        Type2::Typename{ ident, .. } if ident.to_string() == "uint" => Some(0),
        _ => None,
    };
    //todo: read up on other range control operators in CDDL RFC
    // (rangeop / ctlop) S type2
    match operator.operator {
        RangeCtlOp::RangeOp{ is_inclusive, .. } => {
            let range_start = match parent.type2 {
                Type2::UintValue{ value, .. } => *value as isize,
                Type2::IntValue{ value, .. } => *value,
                _ => panic!("Number expected as range start. Found {:?}", parent.type2)
            };
            let range_end = match operator.type2 {
                Type2::UintValue{ value, .. } => value as isize,
                Type2::IntValue{ value, ..} => value,
                _ => unimplemented!("unsupported type in range control operator: {:?}", operator),
            };
            ControlOperator::Range((Some(range_start), Some(if is_inclusive { range_end } else { range_end + 1 })))
        },
        RangeCtlOp::CtlOp{ ctrl, .. } => match ctrl {
            ".default" |
            ".cborseq" |
            ".within" |
            ".and" => todo!("control operator {} not supported", ctrl),
            ".cbor" => ControlOperator::CBOR(rust_type_from_type2(types, &Type2AndParent { type2: &operator.type2, parent: parent.parent, })),
            ".eq" => ControlOperator::Range((Some(type2_to_number_literal(&operator.type2)), Some(type2_to_number_literal(&operator.type2)))),
            // TODO: this would be MUCH nicer (for error displaying, etc) to handle this in its own dedicated way
            //       which might be necessary once we support other control operators anyway
            ".ne" => ControlOperator::Range((Some(type2_to_number_literal(&operator.type2) + 1), Some(type2_to_number_literal(&operator.type2) - 1))),
            ".le" => ControlOperator::Range((lower_bound, Some(type2_to_number_literal(&operator.type2)))),
            ".lt" => ControlOperator::Range((lower_bound, Some(type2_to_number_literal(&operator.type2) - 1))),
            ".ge" => ControlOperator::Range((Some(type2_to_number_literal(&operator.type2)), None)),
            ".gt" => ControlOperator::Range((Some(type2_to_number_literal(&operator.type2) + 1), None)),
            ".size" => {
                let base_range = match &operator.type2 {
                    Type2::UintValue{ value, .. } => ControlOperator::Range((None, Some(*value as isize))),
                    Type2::IntValue{ value, .. } => ControlOperator::Range((None, Some(*value))),
                    Type2::ParenthesizedType{ pt, .. } => {
                        assert_eq!(pt.type_choices.len(), 1);
                        let inner_type = &pt.type_choices.first().unwrap().type1;
                        let min = match inner_type.type2 {
                            Type2::UintValue{ value, .. } => Some(value as isize),
                            Type2::IntValue{ value, .. } => Some(value),
                            _ => unimplemented!("unsupported type in range control operator: {:?}", operator),
                        };
                        let max = match &inner_type.operator {
                            Some(op) => match op.operator {
                                RangeCtlOp::RangeOp{ is_inclusive, ..} => {
                                    let value = match op.type2 {
                                        Type2::UintValue{ value, .. } => value as isize,
                                        Type2::IntValue{ value, ..} => value,
                                        _ => unimplemented!("unsupported type in range control operator: {:?}", operator),
                                    };
                                    Some(if is_inclusive { value } else { value + 1 })
                                },
                                RangeCtlOp::CtlOp{ .. } => panic!(""),
                            },
                            None => min,
                        };
                        ControlOperator::Range((min, max))
                    },
                    _ => unimplemented!("unsupported type in range control operator: {:?}", operator),
                };
                match parent.type2 {
                    Type2::Typename{ ident, .. } if ident.to_string() == "uint" => {
                        // .size 3 means 24 bits
                        match &base_range {
                            ControlOperator::Range((Some(l), Some(h))) => ControlOperator::Range((Some(isize::pow(2, 8 * *l as u32)), Some(isize::pow(2, 8 * *h as u32) - 1))),
                            ControlOperator::Range((None, Some(h))) => ControlOperator::Range((Some(0), Some(isize::pow(2, 8 * *h as u32) - 1))),
                            _ => panic!("unexpected partial range in size control operator: {:?}", operator)
                        }
                    },
                    Type2::Typename{ ident, .. } if ident.to_string() == "int" => {
                        match &base_range {
                            // this is complex to support since it requires two disjoint ranges of possible values
                            ControlOperator::Range((Some(_), Some(_))) => panic!(".size range unsupported for signed int type: {:?}", operator),
                            ControlOperator::Range((None, Some(h))) => ControlOperator::Range((Some(-isize::pow(2, 8 * (*h - 1) as u32)), Some(isize::pow(2, (8 * (*h - 1)) as u32) - 1))),
                            _ => panic!("unexpected partial range in size control operator: {:?}", operator)
                        }
                    }
                    _ => {
                        match base_range {
                            // for strings & byte arrays, specifying an upper value means an exact value (.size 3 means a 3 char string)
                            ControlOperator::Range((None, Some(h))) => ControlOperator::Range((Some(h), Some(h))),
                            range => range,
                        }
                    }
                }
            },
            _ => panic!("Unknown (not seen in RFC-8610) range control operator: {}", ctrl),
        }
    }
}

fn range_to_primitive(low: Option<isize>, high: Option<isize>) -> Option<RustType> {
    match (low, high) {
        (Some(l), Some(h)) if l == u8::MIN as isize && h == u8::MAX as isize => Some(RustType::Primitive(Primitive::U8)),
        (Some(l), Some(h)) if l == i8::MIN as isize && h == i8::MAX as isize => Some(RustType::Primitive(Primitive::I8)),
        (Some(l), Some(h)) if l == u16::MIN as isize && h == u16::MAX as isize => Some(RustType::Primitive(Primitive::U16)),
        (Some(l), Some(h)) if l == i16::MIN as isize && h == i16::MAX as isize => Some(RustType::Primitive(Primitive::I16)),
        (Some(l), Some(h)) if l == u32::MIN as isize && h == u32::MAX as isize => Some(RustType::Primitive(Primitive::U32)),
        (Some(l), Some(h)) if l == i32::MIN as isize && h == i32::MAX as isize => Some(RustType::Primitive(Primitive::I32)),
        (Some(l), Some(h)) if l == u64::MIN as isize && h == u64::MAX as isize => Some(RustType::Primitive(Primitive::U64)),
        (Some(l), Some(h)) if l == i64::MIN as isize && h == i64::MAX as isize => Some(RustType::Primitive(Primitive::I64)),
        _ => None
    }
}

fn parse_type(types: &mut IntermediateTypes, type_name: &RustIdent, type_choice: &TypeChoice, outer_tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let type1 = &type_choice.type1;
    match &type1.type2 {
        Type2::Typename{ ident, generic_args, .. } => {
            // Note: this handles bool constants too, since we apply the type aliases and they resolve
            // and there's no Type2::BooleanValue
            let cddl_ident = CDDLIdent::new(ident.to_string());
            let control = type1.operator.as_ref().map(|op| parse_control_operator(types, &Type2AndParent { type2: &type1.type2, parent: &type1 }, op));
            match control {
                Some(control) => {
                    assert!(generic_params.is_none(), "Generics combined with range specifiers not supported");
                    // TODO: what about aliases that resolve to these? is it even possible to know this at this stage?
                    let field_type = || match cddl_ident.to_string().as_str() {
                        "tstr" | "text" => RustType::Primitive(Primitive::Str),
                        "bstr" | "bytes" => RustType::Primitive(Primitive::Bytes),
                        other => panic!("range control specifiers not supported for type: {}", other),
                    };
                    match control {
                        ControlOperator::Range(min_max) => {
                            match cddl_ident.to_string().as_str() {
                                "int" | "uint" => match range_to_primitive(min_max.0, min_max.1) {
                                    Some(t) => types.register_type_alias(type_name.clone(), t, true, true),
                                    None => panic!("unsupported range for {:?}: {:?}", cddl_ident.to_string().as_str(), control)
                                },
                                _ => types.register_rust_struct(RustStruct::new_wrapper(type_name.clone(), outer_tag, field_type().clone(), Some(min_max)))
                            }
                        },
                        ControlOperator::CBOR(ty) => match field_type() {
                            RustType::Primitive(Primitive::Bytes) => {
                                types.register_type_alias(type_name.clone(), RustType::CBORBytes(Box::new(ty)), true, true);
                            },
                            _ => panic!(".cbor is only allowed on bytes as per CDDL spec"),
                        },
                    }
                },
                None => {
                    let concrete_type = match types.new_type(&cddl_ident) {
                        RustType::Alias(_ident, ty) => *ty,
                        ty => ty,
                    };
                    match &generic_params {
                        Some(_params) => {
                            // this should be the only situation where you need this as otherwise the params would be unbound
                            todo!("generics on defined types e.g. foo<T, U> = [T, U], bar<V> = foo<V, uint>");
                            // TODO: maybe you could do this by resolving it here then storing the resolved one as GenericDef
                        },
                        None => {
                            match generic_args {
                                Some(arg) => {
                                    // This is for named generic instances such as:
                                    // foo = bar<text>
                                    let generic_args = arg.args.iter().map(|a| rust_type_from_type1(types, &a.arg)).collect();
                                    types.register_generic_instance(GenericInstance::new(type_name.clone(), RustIdent::new(cddl_ident.clone()), generic_args))
                                },
                                None => {
                                    let rule_metadata = RuleMetadata::from(type1.comments_after_type.as_ref());
                                    if rule_metadata.is_newtype {
                                        types.register_rust_struct(RustStruct::new_wrapper(type_name.clone(), None, concrete_type, None)); 
                                    } else {
                                        types.register_type_alias(type_name.clone(), concrete_type, true, true);
                                    }
                                }
                            }
                        }
                    }
                },
            }
        },
        Type2::Map{ group, .. } => {
            parse_group(types, group, type_name, Representation::Map, outer_tag, generic_params);
        },
        Type2::Array{ group, .. } => {
            // TODO: We could potentially generate an array-wrapper type around this
            // possibly based on the occurency specifier.
            parse_group(types, group, type_name, Representation::Array, outer_tag, generic_params);
        },
        Type2::TaggedData{ tag, t, .. } => {
            if let Some(_) = outer_tag {
                panic!("doubly nested tags are not supported");
            }
            let tag_unwrap = tag.expect("not sure what empty tag here would mean - unsupported");
            match t.type_choices.len() {
                1 => {
                    let inner_type = &t.type_choices.first().unwrap();
                    match match &inner_type.type1.type2 {
                        Type2::Typename{ ident, .. } => Either::Right(ident),
                        Type2::Map{ group, .. } => Either::Left(group),
                        Type2::Array{ group, .. } => Either::Left(group),
                        x => panic!("only supports tagged arrays/maps/typenames - found: {:?} in rule {}", x, type_name),
                    } {
                        Either::Left(_group) => parse_type(types, type_name, inner_type, *tag, generic_params),
                        Either::Right(ident) => {
                            let new_type = types.new_type(&CDDLIdent::new(ident.to_string()));
                            let control = inner_type.type1.operator.as_ref().map(|op| parse_control_operator(types, &Type2AndParent { parent: &inner_type.type1, type2: &inner_type.type1.type2 }, op));
                            match control {
                                Some(ControlOperator::CBOR(ty)) => {
                                    // TODO: this would be fixed if we ordered definitions via a dependency graph to begin with
                                    // which would also allow us to do a single pass instead of many like we do now
                                    let base_type = types
                                        .apply_type_aliases(&AliasIdent::new(CDDLIdent::new(ident.to_string())))
                                        .expect(&format!("Please move definition for {} above {}", type_name, ident));
                                    types.register_type_alias(type_name.clone(), RustType::Tagged(tag_unwrap, Box::new(RustType::CBORBytes(Box::new(base_type)))), true, true);
                                },
                                Some(ControlOperator::Range(min_max)) => {
                                    match ident.to_string().as_str() {
                                        "int" | "uint" => match range_to_primitive(min_max.0, min_max.1) {
                                            Some(t) => types.register_type_alias(type_name.clone(), t, true, true),
                                            None => panic!("unsupported range for {:?}: {:?}", ident.to_string().as_str(), control)
                                        },
                                        _ => types.register_rust_struct(RustStruct::new_wrapper(type_name.clone(), *tag, new_type, Some(min_max)))
                                    }
                                },
                                None => {
                                    // TODO: this would be fixed if we ordered definitions via a dependency graph to begin with
                                    // which would also allow us to do a single pass instead of many like we do now
                                    let base_type = types
                                        .apply_type_aliases(&AliasIdent::new(CDDLIdent::new(ident.to_string())))
                                        .expect(&format!("Please move definition for {} above {}", type_name, ident));
                                    types.register_type_alias(type_name.clone(), RustType::Tagged(tag_unwrap, Box::new(base_type)), true, true);
                                },
                            }
                        },
                    };
                },
                _ => {
                    parse_type_choices(types, type_name, &t.type_choices, *tag, generic_params);
                }
            };
        },
        // Note: bool constants are handled via Type2::Typename
        Type2::IntValue{ value, .. } => {
            let fallback_type = RustType::Fixed(FixedValue::Int(*value));

            let control = type1.operator.as_ref().map(|op| parse_control_operator(types, &Type2AndParent { parent: type1, type2: &type1.type2 }, op));
            let base_type = match control {
                Some(ControlOperator::Range(min_max)) => {
                    match range_to_primitive(min_max.0, min_max.1) {
                        Some(t) => t,
                        _ => fallback_type
                    }
                },
                _ => fallback_type
            };
            types.register_type_alias(type_name.clone(), base_type, true, true);
        },
        Type2::UintValue{ value, .. } => {
            let fallback_type = RustType::Fixed(FixedValue::Uint(*value));

            let control = type1.operator.as_ref().map(|op| parse_control_operator(types, &Type2AndParent { parent: type1, type2: &type1.type2 }, op));
            let base_type = match control {
                Some(ControlOperator::Range(min_max)) => {
                    match range_to_primitive(min_max.0, min_max.1) {
                        Some(t) => t,
                        _ => fallback_type
                    }
                },
                _ => fallback_type
            };
            types.register_type_alias(type_name.clone(), base_type, true, true);
        },
        Type2::TextValue{ value, .. } => {
            types.register_type_alias(type_name.clone(), RustType::Fixed(FixedValue::Text(value.to_string())), true, true);
        },
        x => {
            panic!("\nignored typename {} -> {:?}\n", type_name, x);
        },
    }
}

// TODO: Also generates individual choices if required, ie for a / [foo] / c would generate Foos
pub fn create_variants_from_type_choices(types: &mut IntermediateTypes, type_choices: &Vec<TypeChoice>) -> Vec<EnumVariant> {
    let mut variant_names_used = BTreeMap::<String, u32>::new();
    type_choices.iter().map(|choice| {
        let rust_type = rust_type_from_type1(types, &choice.type1);
        let variant_name = append_number_if_duplicate(&mut variant_names_used, rust_type.for_variant().to_string());
        EnumVariant::new(VariantIdent::new_custom(variant_name), rust_type, false)
    }).collect()
}

fn table_domain_range<'a>(group_choice: &'a GroupChoice<'a>, rep: Representation) -> Option<(&'a Type1<'a>, &'a Type<'a>)> {
    // Here we test if this is a struct vs a table.
    // struct: { x: int, y: int }, etc
    // table: { * int => tstr }, etc
    // this assumes that all maps representing tables are homogenous
    // and contain no other fields. I am not sure if this is a guarantee in
    // cbor but I would hope that the cddl specs we are using follow this.
    if let Representation::Map = rep {
        if group_choice.group_entries.len() == 1 {
            match group_choice.group_entries.first() {
                Some((GroupEntry::ValueMemberKey{ ge, .. }, _)) => {
                    match &ge.member_key {
                        Some(MemberKey::Type1{ t1, .. }) => {
                            // TODO: Do we need to handle cuts for what we're doing?
                            // Does the range control operator matter?
                            return Some((&t1, &ge.entry_type));
                        },
                        // has a fixed value - this is just a 1-element struct
                        Some(MemberKey::Value{ .. }) => return None,
                        _ => panic!("unsupported table map key (1): {:?}", ge),
                    }
                },
                _ => panic!("unsupported table map key (2): {:?}", group_choice.group_entries.first().unwrap()),
            }
        }
    }
    // Could not get a table-type domain/range - must be a heterogenous struct
    None
}

// would use rust_type_from_type1 but that requires IntermediateTypes which we shouldn't
fn type2_is_null(t2: &Type2) -> bool {
    match t2 {
        Type2::Typename{ ident, .. } => ident.ident == "null" || ident.ident == "nil",
        _ => false,
    }
}

fn type_to_field_name(t: &Type) -> Option<String> {
    let type2_to_field_name = |t2: &Type2| match t2 {
        Type2::Typename{ ident, .. } => Some(ident.to_string()),
            Type2::TextValue { value, .. } => Some(value.to_string()),
            Type2::Array { group, .. } => match group.group_choices.len() {
                1 => {
                    let entries = &group.group_choices.first().unwrap().group_entries;
                    match entries.len() {
                        1 => {
                            match &entries.first().unwrap().0 {
                                // should we do this? here it possibly allows [[foo]] -> fooss
                                GroupEntry::ValueMemberKey{ ge, .. } => Some(format!("{}s", type_to_field_name(&ge.entry_type)?)),
                                GroupEntry::TypeGroupname{ ge, .. } => Some(format!("{}s", ge.name.to_string())),
                                GroupEntry::InlineGroup { .. } => None,
                            }
                        },
                        // only supports homogenous arrays for now
                        _ => None,
                    }
                },
                // no group choice support here
                _ => None
            }
            // non array/text/identifier types not supported here - value keys are caught earlier anyway
            _ => None
    };
    match t.type_choices.len() {
        1 => type2_to_field_name(&t.type_choices.first().unwrap().type1.type2),
        2 => {
            // special case for T / null -> maps to Option<T> so field name should be same as just T
            let a = &t.type_choices[0].type1.type2;
            let b = &t.type_choices[1].type1.type2;
            if type2_is_null(a) {
                type2_to_field_name(b)
            } else if type2_is_null(b) {
                type2_to_field_name(a)
            } else {
                // neither are null - we do not support type choices here
                None
            }
        },
        // no type choice support here
        _ => None,
    }
}

fn _field_name_from_comments<'a>(comments: &Option<Comments<'a>>) -> Option<String> {
    comments
        .as_ref()?
        .0
        .iter()
        .find(|c| c.trim().starts_with("field:"))
        .map(|c| c.trim()[6..].trim().to_owned())
}

fn combine_comments<'a>(a: &'a Option<Comments>, b: &'a Option<Comments>) -> Option<Vec<&'a str>> {
    match (a.as_ref().map(|comment| comment.0.clone()), b.as_ref().map(|comment| comment.0.clone())) {
        (Some(a), Some(b)) => Some([a, b].concat()),
        (opt_a, opt_b) => opt_a.or(opt_b)
    }
}

// Attempts to use the style-converted type name as a field name, and if we have already
// generated one, then we simply add numerals starting at 2, 3, 4...
// If you wish to only check if there is an explicitly stated field name,
// then use group_entry_to_raw_field_name()
fn group_entry_to_field_name(entry: &GroupEntry, index: usize, already_generated: &mut BTreeMap<String, u32>, optional_comma: &OptionalComma) -> String {
    //println!("group_entry_to_field_name() = {:#?}", entry);
    let field_name = convert_to_snake_case(&match entry {
        GroupEntry::ValueMemberKey{ trailing_comments, ge, .. } => match ge.member_key.as_ref() {
            Some(member_key) => match member_key {
                MemberKey::Value{ value, .. } => {
                    let combined_comments = combine_comments(trailing_comments, &optional_comma.trailing_comments);
                    match metadata_from_comments(&combined_comments.unwrap_or_default()) {
                        RuleMetadata { name: Some(name), .. } => name,
                        _ => format!("key_{}", value)
                    }
                },
                MemberKey::Bareword{ ident, .. } => ident.to_string(),
                MemberKey::Type1{ t1, .. } => match t1.type2 {
                    Type2::UintValue{ value, .. } => format!("key_{}", value),
                    _ => panic!("Encountered Type1 member key in multi-field map - not supported: {:?}", entry),
                },
                MemberKey::NonMemberKey{ .. } => panic!("Please open a github issue with repro steps"),
            },
            None => {
                type_to_field_name(&ge.entry_type).unwrap_or_else(|| {
                    let combined_comments = combine_comments(trailing_comments, &optional_comma.trailing_comments);
                    match metadata_from_comments(&combined_comments.unwrap_or_default()) {
                        RuleMetadata { name: Some(name), .. } => name,
                        _ => format!("index_{}", index),
                    }
                })
            }
        },
        GroupEntry::TypeGroupname{ trailing_comments, ge: TypeGroupnameEntry { name, .. }, .. } => match !is_identifier_user_defined(&name.to_string()) {
            true => {
                let combined_comments = combine_comments(trailing_comments, &optional_comma.trailing_comments);
                match metadata_from_comments(&combined_comments.unwrap_or_default()) {
                    RuleMetadata { name: Some(name), .. } => name,
                    _ => format!("index_{}", index),
                }
            },
            false => name.to_string(),
        },
        GroupEntry::InlineGroup{ group, .. } => panic!("not implemented (define a new struct for this!) = {}\n\n {:?}", group, group),
    });
    append_number_if_duplicate(already_generated, field_name.clone())
}

// Only returns Some(String) if there was an explicit field name provided, otherwise None.
// If you need to try and make one using the type/etc, then try group_entry_to_field_name()
// Also does not do any CamelCase or snake_case formatting.
fn group_entry_to_raw_field_name(entry: &GroupEntry) -> Option<String> {
    match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => match ge.member_key.as_ref() {
            Some(MemberKey::Bareword{ ident, .. } ) => Some(ident.to_string()),
            _ => None,
        },
        GroupEntry::TypeGroupname{ ge: TypeGroupnameEntry { name, .. }, .. } => match !is_identifier_user_defined(&name.to_string()) {
            true => None,
            false => Some(name.to_string()),
        },
        GroupEntry::InlineGroup{ group, .. } => panic!("not implemented (define a new struct for this!) = {}\n\n {:?}", group, group),
    }
}

fn rust_type_from_type1(types: &mut IntermediateTypes, type1: &Type1) -> RustType {
    let control = type1.operator.as_ref().map(|op| parse_control_operator(types, &Type2AndParent { parent: type1, type2: &type1.type2 }, op));
    // println!("type1: {:#?}", type1);
    match control {
        Some(ControlOperator::CBOR(ty)) => RustType::CBORBytes(Box::new(ty)),
        Some(ControlOperator::Range(min_max)) => {
            match &type1.type2 {
                Type2::Typename{ ident, .. } if ident.to_string() == "uint" || ident.to_string() == "int" => match range_to_primitive(min_max.0, min_max.1) {
                    Some(t) => t,
                    None => panic!("unsupported range for {:?}: {:?}", ident.to_string().as_str(), control)
                },
                _ => rust_type_from_type2(types, &Type2AndParent { type2: &type1.type2, parent: type1, })
            }
        },
        _ => rust_type_from_type2(types, &Type2AndParent { type2: &type1.type2, parent: type1, })
    }
}

fn rust_type_from_type2(types: &mut IntermediateTypes, type2: &Type2AndParent) -> RustType {
    // TODO: socket plugs (used in hash type)
    match &type2.type2 {
        Type2::UintValue{ value, .. } => RustType::Fixed(FixedValue::Uint(*value)),
        Type2::IntValue{ value, .. } => RustType::Fixed(FixedValue::Int(*value)),
        //Type2::FloatValue{ value, .. } => RustType::Fixed(FixedValue::Float(*value)),
        Type2::TextValue{ value, .. } => RustType::Fixed(FixedValue::Text(value.to_string())),
        Type2::Typename{ ident, generic_args, .. } => {
            let cddl_ident = CDDLIdent::new(ident.ident);
            match generic_args {
                Some(args) => {
                    // This is for anonymous instances (i.e. members) such as:
                    // foo = [a: bar<text, bool>]
                    // so to be able to expose it to wasm, we create a new generic instance
                    // under the name bar_string_bool in this case.
                    let generic_args = args.args.iter().map(|a| rust_type_from_type1(types, &a.arg)).collect::<Vec<_>>();
                    let args_name = generic_args.iter().map(|t| t.for_variant().to_string()).collect::<Vec<String>>().join("_");
                    let instance_cddl_ident = CDDLIdent::new(format!("{}_{}", cddl_ident, args_name));
                    let instance_ident = RustIdent::new(instance_cddl_ident.clone());
                    let generic_ident = RustIdent::new(cddl_ident);
                    types.register_generic_instance(GenericInstance::new(instance_ident, generic_ident, generic_args));
                    types.new_type(&instance_cddl_ident)
                },
                None => types.new_type(&cddl_ident),
            }
        },
        Type2::Array{ group, .. } => {
            // TODO: support for group choices in arrays?
            let element_type = match group.group_choices.len() {
                1 => {
                    let choice = &group.group_choices.first().unwrap();
                    // special case for homogenous arrays
                    if choice.group_entries.len() == 1 {
                        let (entry, _has_comma) = choice.group_entries.first().unwrap();
                        match entry {
                            GroupEntry::ValueMemberKey{ ge, .. } => rust_type(types, &ge.entry_type),
                            GroupEntry::TypeGroupname{ ge, .. } => types.new_type(&CDDLIdent::new(&ge.name.to_string())),
                            _ => panic!("UNSUPPORTED_ARRAY_ELEMENT<{:?}>", entry),
                        }
                    } else {
                        let rule_metadata = RuleMetadata::from(type2.parent.comments_after_type.as_ref());
                        let name = match rule_metadata.name.as_ref() {
                            Some(name) => name,
                            None => panic!("Anonymous groups not allowed. Either create an explicit rule (foo = [0, bytes]) or give it a name using the @name notation. Group: {:#?}", group)
                        };
                        let cddl_ident = CDDLIdent::new(name);
                        let rust_ident = RustIdent::new(cddl_ident.clone());
                        parse_group(types, group, &rust_ident, Representation::Array, None, None);
                        // we aren't returning an array, but rather a struct where the fields are ordered
                        return types.new_type(&cddl_ident)
                    }
                },
                // array of elements with choices: enums?
                _ => unimplemented!("group choices in array type not supported"),
            };
            
            //let array_wrapper_name = element_type.name_as_wasm_array();
            //types.create_and_register_array_type(element_type, &array_wrapper_name)
            RustType::Array(Box::new(element_type))
        },
        Type2::Map { group, .. } => {
            match group.group_choices.len() {
                1 => {
                    let group_choice = group.group_choices.first().unwrap();
                    let table_types = table_domain_range(group_choice, Representation::Map);
                    match table_types {
                        // Table map - homogenous key/value types
                        Some((domain, range)) => {
                            let key_type = rust_type_from_type1(types, domain);
                            let value_type = rust_type(types, range);
                            // Generate a MapTToV for a { t => v } table-type map as we are an anonymous type
                            // defined as part of another type if we're in this level of parsing.
                            // We also can't have plain groups unlike arrays, so don't try and generate those
                            // for general map types we can though but not for tables
                            //let table_type_ident = RustIdent::new(CDDLIdent::new(format!("Map{}To{}", key_type.for_wasm_member(), value_type.for_wasm_member())));
                            //types.register_rust_struct(RustStruct::new_table(table_type_ident, None, key_type.clone(), value_type.clone()));
                            RustType::Map(Box::new(key_type), Box::new(value_type))
                        },
                        None => unimplemented!("TODO: non-table types as types: {:?}", group),
                    }
                },
                _ => unimplemented!("group choices in inlined map types not allowed: {:?}", group),
            }
        },
        // unsure if we need to handle the None case - when does this happen?
        Type2::TaggedData{ tag, t, .. } => {
            RustType::Tagged(tag.expect("tagged data without tag not supported"), Box::new(rust_type(types, t)))
        },
        _ => {
            panic!("Ignoring Type2: {:?}", type2.type2);
        },
    }
}

fn rust_type(types: &mut IntermediateTypes, t: &Type) -> RustType {
    if t.type_choices.len() == 1 {
        rust_type_from_type1(types, &t.type_choices.first().unwrap().type1)
    } else {
        if t.type_choices.len() == 2 {
            // T / null   or   null / T   should map to Option<T>
            let a = &t.type_choices[0].type1;
            let b = &t.type_choices[1].type1;
            if type2_is_null(&a.type2) {
                return RustType::Optional(Box::new(rust_type_from_type1(types, b)));
            }
            if type2_is_null(&b.type2) {
                return RustType::Optional(Box::new(rust_type_from_type1(types, a)));
            }
        }
        let variants = create_variants_from_type_choices(types, &t.type_choices);
        let mut combined_name = String::new();
        // one caveat: nested types can leave ambiguous names and cause problems like
        // (a / b) / c and a / (b / c) would both be AOrBOrC
        for variant in &variants {
            if !combined_name.is_empty() {
                combined_name.push_str("Or");
            }
            // due to undercase primitive names, we need to convert here
            combined_name.push_str(&variant.rust_type.for_variant().to_string());
        }
        let combined_ident = RustIdent::new(CDDLIdent::new(&combined_name));
        types.register_rust_struct(RustStruct::new_type_choice(combined_ident, None, variants));
        types.new_type(&CDDLIdent::new(combined_name))
    }
}

fn group_entry_optional(entry: &GroupEntry) -> bool {
    let occur = match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => &ge.occur,
        GroupEntry::TypeGroupname{ ge, .. } => &ge.occur,
        GroupEntry::InlineGroup{ .. } => panic!("inline group entries are not implemented"),
    };
    occur
        .as_ref()
        .map(|o| match o.occur {
            Occur::Optional(_) => true,
            _ => false,
        })
        .unwrap_or(false)
}

fn group_entry_to_type(types: &mut IntermediateTypes, entry: &GroupEntry) -> RustType {
    let ret = match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => rust_type(types, &ge.entry_type),
        GroupEntry::TypeGroupname{ ge, .. } => {
            if ge.generic_args.is_some() {
                // I am not sure how we end up with this kind of generic args since definitional ones
                // and member ones are created elsewhere. I thought that if you had a field like
                // foo: bar<uint> it would be here but it turns out it's in the ValueMemberKey
                // variant instead.
                todo!("If you run into this please create a github issue and include the .cddl that caused it");
            }
            let cddl_ident = CDDLIdent::new(ge.name.to_string());
            types.new_type(&cddl_ident)
        },
        GroupEntry::InlineGroup{ .. } => panic!("inline group entries are not implemented"),
    };
    //println!("group_entry_to_typename({:?}) = {:?}\n", entry, ret);
    ret
}

fn group_entry_to_key(entry: &GroupEntry) -> Option<FixedValue> {
    match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => {
            match ge.member_key.as_ref()? {
                MemberKey::Value{ value, .. } => match value {
                    cddl::token::Value::UINT(x) => Some(FixedValue::Uint(*x)),
                    cddl::token::Value::INT(x) => Some(FixedValue::Int(*x)),
                    cddl::token::Value::TEXT(x) => Some(FixedValue::Text(x.to_string())),
                    _ => panic!("unsupported map identifier(1): {:?}", value),
                },
                MemberKey::Bareword{ ident, .. } => Some(FixedValue::Text(ident.to_string())),
                MemberKey::Type1{ t1, .. } => match &t1.type2 {
                    Type2::UintValue{ value, .. } => Some(FixedValue::Uint(*value)),
                    Type2::IntValue{ value, .. } => Some(FixedValue::Int(*value)),
                    Type2::TextValue{ value, .. } => Some(FixedValue::Text(value.to_string())),
                    _ => panic!("unsupported map identifier(2): {:?}", entry),
                },
                MemberKey::NonMemberKey{ .. } => panic!("Please open a github issue with repro steps"),
            }
        },
        _ => None,
    }
}

fn parse_record_from_group_choice(types: &mut IntermediateTypes, rep: Representation, group_choice: &GroupChoice) -> RustRecord {
    let mut generated_fields = BTreeMap::<String, u32>::new();
    let fields = group_choice.group_entries.iter().enumerate().map(
        |(index, (group_entry, optional_comma))| {
            let field_name = group_entry_to_field_name(group_entry, index, &mut generated_fields, optional_comma);
            // does not exist for fixed values importantly
            let field_type = group_entry_to_type(types, group_entry);
            if let RustType::Rust(ident) = &field_type {
                types.set_rep_if_plain_group(ident, rep);
            }
            let optional_field = group_entry_optional(group_entry);
            let key = match rep {
                Representation::Map => Some(group_entry_to_key(group_entry).expect("map fields need keys")),
                Representation::Array => None,
            };
            RustField::new(field_name, field_type, optional_field, key)
        }
    ).collect();
    RustRecord {
        rep,
        fields,
    }
}

fn parse_group_choice<'a>(types: &mut IntermediateTypes, group_choice: &'a GroupChoice, name: &RustIdent, rep: Representation, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let table_types = table_domain_range(group_choice, rep);
    let rust_struct = match table_types {
        // Table map - homogenous key/value types
        Some((domain, range)) => {
            let key_type = rust_type_from_type1(types, domain);
            let value_type = rust_type(types, range);
            RustStruct::new_table(name.clone(), tag, key_type, value_type)
        },
        // Heterogenous map (or array!) with defined key/value pairs in the cddl like a struct
        None => {
            let record = parse_record_from_group_choice(types, rep, group_choice);
            // We need to store this in IntermediateTypes so we can refer from one struct to another.
            RustStruct::new_record(name.clone(), tag, record)
        }
    };
    match generic_params {
        Some(params) => types.register_generic_def(GenericDef::new(params, rust_struct)),
        None => types.register_rust_struct(rust_struct),
    };
}

pub fn parse_group(types: &mut IntermediateTypes, group: &Group, name: &RustIdent, rep: Representation, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    if group.group_choices.len() == 1 {
        // Handle simple (no choices) group.
        parse_group_choice(types, group.group_choices.first().unwrap(), name, rep, tag, generic_params);
    } else {
        if generic_params.is_some() {
            todo!("{}: generic group choices not supported", name);
        }
        // Generate Enum object that is not exposed to wasm, since wasm can't expose
        // fully featured rust enums via wasm_bindgen

        // TODO: We don't support generating SerializeEmbeddedGroup for group choices which is necessary for plain groups
        // It would not be as trivial to add as we do the outer group's array/map tag writing inside the variant match
        // to avoid having to always generate SerializeEmbeddedGroup when not necessary.
        assert!(!types.is_plain_group(name));
        
        // Handle group with choices by generating an enum then generating a group for every choice
        let mut variants_names_used = BTreeMap::<String, u32>::new();
        let variants: Vec<EnumVariant> = group.group_choices.iter().enumerate().map(|(i, group_choice)| {
            // If we're a 1-element we should just wrap that type in the variant rather than
            // define a new struct just for each variant.
            // TODO: handle map-based enums? It would require being able to extract the key logic
            // We might end up doing this anyway to support table-maps in choices though.
            if group_choice.group_entries.len() == 1 {
                let group_entry = &group_choice.group_entries.first().unwrap().0;
                let ty = group_entry_to_type(types, group_entry);
                let serialize_as_embedded = if let RustType::Rust(ident) = &ty {
                    // we might need to generate it if not used elsewhere
                    types.set_rep_if_plain_group(ident, rep);
                    types.is_plain_group(ident)
                } else {
                    false
                };
                let variant_ident = convert_to_camel_case(&match group_entry_to_raw_field_name(group_entry) {
                    Some(name) => name,
                    None => append_number_if_duplicate(&mut variants_names_used, ty.for_variant().to_string()),
                });
                let variant_ident = VariantIdent::new_custom(variant_ident);
                EnumVariant::new(variant_ident, ty, serialize_as_embedded)
                // None => {
                //     // TODO: Weird case, group choice with only one fixed-value field.
                //     // What should we do here? In the future we could make this a
                //     // non-value-taking enum then handle this in the serialization code.
                //     // However, for now we just default to default behavior:
                //     let variant_name = format!("{}{}", name, i);
                //     // TODO: Should we generate these within their own namespace?
                //     codegen_group_choice(global, group_choice, &variant_name, rep, None);
                //     EnumVariant::new(variant_name.clone(), RustType::Rust(variant_name), true)
                // },
            } else {
                let rule_metadata = RuleMetadata::from(group_choice.comments_before_grpchoice.as_ref());
                let ident_name = rule_metadata.name.unwrap_or_else(|| format!("{}{}", name, i));
                // General case, GroupN type identifiers and generate group choice since it's inlined here
                let variant_name = RustIdent::new(CDDLIdent::new(ident_name));
                types.mark_plain_group(variant_name.clone(), None);
                parse_group_choice(types, group_choice, &variant_name, rep, None, generic_params.clone());
                EnumVariant::new(VariantIdent::new_rust(variant_name.clone()), RustType::Rust(variant_name), true)
            }
        }).collect();
        types.register_rust_struct(RustStruct::new_group_choice(name.clone(), tag, variants, rep));
    }
}
