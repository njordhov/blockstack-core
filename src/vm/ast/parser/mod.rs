use std::cmp;
use std::convert::TryInto;
use util::hash::hex_bytes;
use regex::{Regex, Captures};
use address::c32::c32_address_decode;
use vm::ast::errors::{ParseResult, ParseErrors, ParseError};
use vm::errors::{RuntimeErrorType, InterpreterResult as Result};
use vm::representations::{PreSymbolicExpression, PreSymbolicExpressionType, ContractName, ClarityName, MAX_STRING_LEN};
use vm::types::{Value, PrincipalData, TraitIdentifier, QualifiedContractIdentifier};

pub const CONTRACT_MIN_NAME_LENGTH : usize = 5;
pub const CONTRACT_MAX_NAME_LENGTH : usize = 40;

pub enum LexItem {
    LeftParen,
    RightParen,
    LeftCurly,
    RightCurly,
    LiteralValue(usize, Value),
    SugaredContractIdentifier(usize, ContractName),
    SugaredFieldIdentifier(usize, ContractName, ClarityName),
    FieldIdentifier(usize, TraitIdentifier),
    TraitReference(usize, ClarityName),
    Variable(String),
    CommaSeparator,
    ColonSeparator,
    Whitespace
}

#[derive(Debug)]
enum TokenType {
    Whitespace, Comma, Colon,
    LParens, RParens,
    LCurly, RCurly,
    StringLiteral, HexStringLiteral,
    UIntLiteral, IntLiteral, QuoteLiteral,
    Variable, TraitReferenceLiteral, PrincipalLiteral,
    SugaredContractIdentifierLiteral,
    FullyQualifiedContractIdentifierLiteral,
    SugaredFieldIdentifierLiteral,
    FullyQualifiedFieldIdentifierLiteral,
}

struct LexMatcher {
    matcher: Regex,
    handler: TokenType
}

enum LexContext {
    ExpectNothing,
    ExpectClosing
}

enum ParseContext {
    CollectList,
    CollectTuple,
}

impl LexMatcher {
    fn new(regex_str: &str, handles: TokenType) -> LexMatcher {
        LexMatcher {
            matcher: Regex::new(&format!("^{}", regex_str)).unwrap(),
            handler: handles
        }
    }
}

fn get_value_or_err(input: &str, captures: Captures) -> ParseResult<String> {
    let matched = captures.name("value").ok_or(
        ParseError::new(ParseErrors::FailedCapturingInput))?;
    Ok(input[matched.start()..matched.end()].to_string())
}

fn get_lines_at(input: &str) -> Vec<usize> {
    let mut out: Vec<_> = input.match_indices("\n")
        .map(|(ix, _)| ix)
        .collect();
    out.reverse();
    out
}

pub fn lex(input: &str) -> ParseResult<Vec<(LexItem, u32, u32)>> {
    // Aaron: I'd like these to be static, but that'd require using
    //    lazy_static (or just hand implementing that), and I'm not convinced
    //    it's worth either (1) an extern macro, or (2) the complexity of hand implementing.

    let lex_matchers: &[LexMatcher] = &[
        LexMatcher::new(r##""(?P<value>((\\")|([[ -~]&&[^"]]))*)""##, TokenType::StringLiteral),
        LexMatcher::new(";;[ -~]*", TokenType::Whitespace), // ;; comments.
        LexMatcher::new("[\n]+", TokenType::Whitespace),
        LexMatcher::new("[ \t]+", TokenType::Whitespace),
        LexMatcher::new("[,]", TokenType::Comma),
        LexMatcher::new("[:]", TokenType::Colon),
        LexMatcher::new("[(]", TokenType::LParens),
        LexMatcher::new("[)]", TokenType::RParens),
        LexMatcher::new("[{]", TokenType::LCurly),
        LexMatcher::new("[}]", TokenType::RCurly),
        LexMatcher::new("<(?P<value>([[:word:]]|[-])+)>", TokenType::TraitReferenceLiteral),
        LexMatcher::new("0x(?P<value>[[:xdigit:]]+)", TokenType::HexStringLiteral),
        LexMatcher::new("u(?P<value>[[:digit:]]+)", TokenType::UIntLiteral),
        LexMatcher::new("(?P<value>-?[[:digit:]]+)", TokenType::IntLiteral),
        LexMatcher::new("'(?P<value>true|false)", TokenType::QuoteLiteral),
        LexMatcher::new(&format!(r#"'(?P<value>[0123456789ABCDEFGHJKMNPQRSTVWXYZ]{{28,41}}(\.)([[:alnum:]]|[-]){{{},{}}}(\.)([[:alnum:]]|[-]){{1,{}}})"#,
            CONTRACT_MIN_NAME_LENGTH, CONTRACT_MAX_NAME_LENGTH, MAX_STRING_LEN),
            TokenType::FullyQualifiedFieldIdentifierLiteral),
        LexMatcher::new(&format!(r#"(?P<value>(\.)([[:alnum:]]|[-]){{{},{}}}(\.)([[:alnum:]]|[-]){{1,{}}})"#,
            CONTRACT_MIN_NAME_LENGTH, CONTRACT_MAX_NAME_LENGTH, MAX_STRING_LEN), TokenType::SugaredFieldIdentifierLiteral),
        LexMatcher::new(&format!(r#"'(?P<value>[0123456789ABCDEFGHJKMNPQRSTVWXYZ]{{28,41}}(\.)([[:alnum:]]|[-]){{{},{}}})"#,
            CONTRACT_MIN_NAME_LENGTH, CONTRACT_MAX_NAME_LENGTH), TokenType::FullyQualifiedContractIdentifierLiteral),
        LexMatcher::new(&format!(r#"(?P<value>(\.)([[:alnum:]]|[-]){{{},{}}})"#,
            CONTRACT_MIN_NAME_LENGTH, CONTRACT_MAX_NAME_LENGTH), TokenType::SugaredContractIdentifierLiteral),
        LexMatcher::new("'(?P<value>[0123456789ABCDEFGHJKMNPQRSTVWXYZ]{28,41})", TokenType::PrincipalLiteral),
        LexMatcher::new("(?P<value>([[:word:]]|[-!?+<>=/*])+)", TokenType::Variable),
    ];

    let mut context = LexContext::ExpectNothing;

    let mut line_indices = get_lines_at(input);
    let mut next_line_break = line_indices.pop();
    let mut current_line: u32 = 1;

    let mut result = Vec::new();
    let mut munch_index = 0;
    let mut column_pos: u32 = 1;
    let mut did_match = true;
    while did_match && munch_index < input.len() {
        if let Some(next_line_ix) = next_line_break {
            if munch_index > next_line_ix {
                next_line_break = line_indices.pop();
                column_pos = 1;
                current_line = current_line.checked_add(1)
                    .ok_or(ParseError::new(ParseErrors::ProgramTooLarge))?;
            }
        }

        did_match = false;
        let current_slice = &input[munch_index..];
        for matcher in lex_matchers.iter() {
            if let Some(captures) = matcher.matcher.captures(current_slice) {
                let whole_match = captures.get(0).unwrap();
                assert_eq!(whole_match.start(), 0);
                munch_index += whole_match.end();

                match context {
                    LexContext::ExpectNothing => Ok(()),
                    LexContext::ExpectClosing => {
                        // expect the next lexed item to be something that typically
                        // "closes" an atom -- i.e., whitespace or a right-parens.
                        // this prevents an atom like 1234abc from getting split into "1234" and "abc"
                        match matcher.handler {
                            TokenType::RParens => Ok(()),
                            TokenType::RCurly => Ok(()),
                            TokenType::Whitespace => Ok(()),
                            TokenType::Comma => Ok(()),
                            TokenType::Colon => Ok(()),
                            _ => Err(ParseError::new(ParseErrors::SeparatorExpected(current_slice[..whole_match.end()].to_string())))
                        }
                    }
                }?;

                // default to expect a closing
                context = LexContext::ExpectClosing;

                let token = match matcher.handler {
                    TokenType::LParens => {
                        context = LexContext::ExpectNothing;
                        Ok(LexItem::LeftParen)
                    },
                    TokenType::RParens => {
                        Ok(LexItem::RightParen)
                    },
                    TokenType::Whitespace => {
                        context = LexContext::ExpectNothing;
                        Ok(LexItem::Whitespace)
                    },
                    TokenType::Comma => {
                        context = LexContext::ExpectNothing;
                        Ok(LexItem::CommaSeparator)
                    },
                    TokenType::Colon => {
                        context = LexContext::ExpectNothing;
                        Ok(LexItem::ColonSeparator)
                    },
                    TokenType::LCurly => {
                        context = LexContext::ExpectNothing;
                        Ok(LexItem::LeftCurly)
                    },
                    TokenType::RCurly => {
                        Ok(LexItem::RightCurly)
                    },
                    TokenType::Variable => {
                        let value = get_value_or_err(current_slice, captures)?;
                        if value.contains("#") {
                            Err(ParseError::new(ParseErrors::IllegalVariableName(value)))
                        } else {
                            Ok(LexItem::Variable(value))
                        }
                    },
                    TokenType::QuoteLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match str_value.as_str() {
                            "true" => Ok(Value::Bool(true)),
                            "false" => Ok(Value::Bool(false)),
                            _ => Err(ParseError::new(ParseErrors::UnknownQuotedValue(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::UIntLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match u128::from_str_radix(&str_value, 10) {
                            Ok(parsed) => Ok(Value::UInt(parsed)),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingIntValue(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::IntLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match i128::from_str_radix(&str_value, 10) {
                            Ok(parsed) => Ok(Value::Int(parsed)),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingIntValue(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::FullyQualifiedContractIdentifierLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match PrincipalData::parse_qualified_contract_principal(&str_value) {
                            Ok(parsed) => Ok(Value::Principal(parsed)),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingPrincipal(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::SugaredContractIdentifierLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match str_value[1..].to_string().try_into() {
                            Ok(parsed) => Ok(parsed),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingPrincipal(str_value.clone())))
                        }?;
                        Ok(LexItem::SugaredContractIdentifier(str_value.len(), value))
                    },
                    TokenType::FullyQualifiedFieldIdentifierLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match TraitIdentifier::parse_fully_qualified(&str_value) {
                            Ok(parsed) => Ok(parsed),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingField(str_value.clone())))
                        }?;
                        Ok(LexItem::FieldIdentifier(str_value.len(), value))
                    },
                    TokenType::SugaredFieldIdentifierLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let (contract_name, field_name) = match TraitIdentifier::parse_sugared_syntax(&str_value) {
                            Ok((contract_name, field_name)) => Ok((contract_name, field_name)),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingField(str_value.clone())))
                        }?;
                        Ok(LexItem::SugaredFieldIdentifier(str_value.len(), contract_name, field_name))
                    },
                    TokenType::PrincipalLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let value = match PrincipalData::parse_standard_principal(&str_value) {
                            Ok(parsed) => Ok(Value::Principal(PrincipalData::Standard(parsed))),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingPrincipal(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::TraitReferenceLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let data = str_value.clone().try_into()
                            .map_err(|_| { ParseError::new(ParseErrors::IllegalVariableName(str_value.to_string())) })?;
                        Ok(LexItem::TraitReference(str_value.len(), data))
                    },
                    TokenType::HexStringLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let byte_vec = hex_bytes(&str_value)
                            .map_err(|x| { ParseError::new(ParseErrors::FailedParsingHexValue(str_value.clone(), x.to_string())) })?;
                        let value = match Value::buff_from(byte_vec) {
                            Ok(parsed) => Ok(parsed),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingBuffer(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                    TokenType::StringLiteral => {
                        let str_value = get_value_or_err(current_slice, captures)?;
                        let quote_unescaped = str_value.replace("\\\"","\"");
                        let slash_unescaped = quote_unescaped.replace("\\\\","\\");
                        let byte_vec = slash_unescaped.as_bytes().to_vec();
                        let value = match Value::buff_from(byte_vec) {
                            Ok(parsed) => Ok(parsed),
                            Err(_e) => Err(ParseError::new(ParseErrors::FailedParsingBuffer(str_value.clone())))
                        }?;
                        Ok(LexItem::LiteralValue(str_value.len(), value))
                    },
                }?;

                result.push((token, current_line, column_pos));
                column_pos += whole_match.end() as u32;
                did_match = true;
                break;
            }
        }
    }

    if munch_index == input.len() {
        Ok(result)
    } else {
        Err(ParseError::new(ParseErrors::FailedParsingRemainder(input[munch_index..].to_string())))
    }
}

pub fn parse_lexed(mut input: Vec<(LexItem, u32, u32)>) -> ParseResult<Vec<PreSymbolicExpression>> {
    let mut parse_stack = Vec::new();

    let mut output_list = Vec::new();

    for (item, line_pos, column_pos) in input.drain(..) {
        match item {
            LexItem::LeftParen => {
                // start new list.
                let new_list = Vec::new();
                parse_stack.push((new_list, line_pos, column_pos, ParseContext::CollectList));
            },
            LexItem::RightParen => {
                // end current list.
                if let Some((value, start_line, start_column, parse_context)) = parse_stack.pop() {
                    match parse_context {
                        ParseContext::CollectList => {
                            let mut pre_expr = PreSymbolicExpression::list(value.into_boxed_slice());
                            pre_expr.set_span(start_line, start_column, line_pos, column_pos);
                            match parse_stack.last_mut() {
                                None => {
                                    // no open lists on stack, add current to result.
                                    output_list.push(pre_expr)
                                },
                                Some((ref mut list, _, _, _)) => {
                                    list.push(pre_expr);
                                }
                            };
                        },
                        ParseContext::CollectTuple => {
                            return Err(ParseError::new(ParseErrors::ClosingTupleLiteralExpected))
                        }
                    }
                } else {
                    return Err(ParseError::new(ParseErrors::ClosingParenthesisUnexpected))
                }
            },
            LexItem::LeftCurly => {
                let new_list = Vec::new();
                parse_stack.push((new_list, line_pos, column_pos, ParseContext::CollectTuple));
            },
            LexItem::RightCurly => {
                if let Some((value, start_line, start_column, parse_context)) = parse_stack.pop() {
                    match parse_context {
                        ParseContext::CollectTuple => {
                            let mut pre_expr = PreSymbolicExpression::tuple(value.into_boxed_slice());
                            pre_expr.set_span(start_line, start_column, line_pos, column_pos);
                            match parse_stack.last_mut() {
                                None => {
                                    output_list.push(pre_expr)
                                },
                                Some((ref mut list, _, _, _)) => {
                                    list.push(pre_expr);
                                }
                            };
                        },
                        ParseContext::CollectList => {
                            return Err(ParseError::new(ParseErrors::ClosingParenthesisExpected))
                        }
                    }
                } else {
                    return Err(ParseError::new(ParseErrors::ClosingTupleLiteralUnexpected))
                }
            },
            LexItem::Variable(value) => {
                let end_column = column_pos + (value.len() as u32) - 1;
                let value = value.clone().try_into()
                    .map_err(|_| { ParseError::new(ParseErrors::IllegalVariableName(value.to_string())) })?;
                let mut pre_expr = PreSymbolicExpression::atom(value);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            },
            LexItem::LiteralValue(length, value) => {
                let mut end_column = column_pos + (length as u32);
                // Avoid underflows on cases like empty strings
                if length > 0 {
                    end_column = end_column - 1;
                }
                let mut pre_expr = PreSymbolicExpression::atom_value(value);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            },
            LexItem::SugaredContractIdentifier(length, value) => {
                let mut end_column = column_pos + (length as u32);
                // Avoid underflows on cases like empty strings
                if length > 0 {
                    end_column = end_column - 1;
                }
                let mut pre_expr = PreSymbolicExpression::sugared_contract_identifier(value);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            },
            LexItem::SugaredFieldIdentifier(length, contract_name, name) => {
                let mut end_column = column_pos + (length as u32);
                // Avoid underflows on cases like empty strings
                if length > 0 {
                    end_column = end_column - 1;
                }
                let mut pre_expr = PreSymbolicExpression::sugared_field_identifier(contract_name, name);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            },
            LexItem::FieldIdentifier(length, trait_identifier) => {
                let mut end_column = column_pos + (length as u32);
                // Avoid underflows on cases like empty strings
                if length > 0 {
                    end_column = end_column - 1;
                }
                let mut pre_expr = PreSymbolicExpression::field_identifier(trait_identifier);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            },
            LexItem::TraitReference(_length, value) => {
                let end_column = column_pos + (value.len() as u32) - 1;
                let value = value.clone().try_into()
                    .map_err(|_| { ParseError::new(ParseErrors::IllegalVariableName(value.to_string())) })?;
                let mut pre_expr = PreSymbolicExpression::trait_reference(value);
                pre_expr.set_span(line_pos, column_pos, line_pos, end_column);

                match parse_stack.last_mut() {
                    None => output_list.push(pre_expr),
                    Some((ref mut list, _, _, _)) => list.push(pre_expr)
                };
            }
            LexItem::ColonSeparator => {
                match parse_stack.last_mut() {
                    None => return Err(ParseError::new(ParseErrors::ColonSeparatorUnexpected)),
                    Some((ref mut list, _, _, parse_context)) => {
                        if let ParseContext::CollectTuple = parse_context {
                            if list.len() % 2 == 0 {
                                return Err(ParseError::new(ParseErrors::ColonSeparatorUnexpected))
                            }
                        } else {
                            return Err(ParseError::new(ParseErrors::ColonSeparatorUnexpected))
                        }
                    }
                };
            }
            LexItem::CommaSeparator => {
                match parse_stack.last_mut() {
                    None => return Err(ParseError::new(ParseErrors::CommaSeparatorUnexpected)),
                    Some((ref mut list, _, _, parse_context)) => {
                        if let ParseContext::CollectTuple = parse_context {
                            if list.len() == 0 || list.len() % 2 == 1 {
                                return Err(ParseError::new(ParseErrors::CommaSeparatorUnexpected))
                            }
                        } else {
                            return Err(ParseError::new(ParseErrors::CommaSeparatorUnexpected))
                        }
                    }
                };
            },
            LexItem::Whitespace => ()
        };
    }

    // check unfinished stack:
    if parse_stack.len() > 0 {
        Err(ParseError::new(ParseErrors::ClosingParenthesisExpected))
    } else {
        Ok(output_list)
    }
}

pub fn parse(input: &str) -> ParseResult<Vec<PreSymbolicExpression>> {
    let lexed = lex(input)?;
    parse_lexed(lexed)
}


#[cfg(test)]
mod test {
    use vm::representations::{PreSymbolicExpression, PreSymbolicExpressionType};
    use vm::{Value, ast};
    use vm::types::{QualifiedContractIdentifier, PrincipalData};
    use vm::ast::errors::{ParseErrors, ParseError};
    use vm::types::{TraitIdentifier};

    fn make_atom(x: &str, start_line: u32, start_column: u32, end_line: u32, end_column: u32) -> PreSymbolicExpression {
        let mut e = PreSymbolicExpression::atom(x.into());
        e.set_span(start_line, start_column, end_line, end_column);
        e
    }

    fn make_atom_value(x: Value, start_line: u32, start_column: u32, end_line: u32, end_column: u32) -> PreSymbolicExpression {
        let mut e = PreSymbolicExpression::atom_value(x);
        e.set_span(start_line, start_column, end_line, end_column);
        e
    }

    fn make_list(start_line: u32, start_column: u32, end_line: u32, end_column: u32, x: Box<[PreSymbolicExpression]>) -> PreSymbolicExpression {
        let mut e = PreSymbolicExpression::list(x);
        e.set_span(start_line, start_column, end_line, end_column);
        e
    }

    fn make_tuple(start_line: u32, start_column: u32, end_line: u32, end_column: u32, x: Box<[PreSymbolicExpression]>) -> PreSymbolicExpression {
        let mut e = PreSymbolicExpression::tuple(x);
        e.set_span(start_line, start_column, end_line, end_column);
        e
    }

    #[test]
    fn test_parse_let_expression() {

        // This test includes some assertions ont the spans of each atom / atom_value / list, which makes indentation important.
        let input =
r#"z (let ((x 1) (y 2))
    (+ x ;; "comments section?"
        ;; this is also a comment!
        (let ((x 3)) ;; more commentary
        (+ x y))
        x)) x y
        ;; this is 'quoted comment!"#;
        let program = vec![
            make_atom("z", 1, 1, 1, 1),
            make_list(1, 3, 6, 11, Box::new([
                make_atom("let", 1, 4, 1, 6),
                make_list(1, 8, 1, 20, Box::new([
                    make_list(1, 9, 1, 13, Box::new([
                        make_atom("x", 1, 10, 1, 10),
                        make_atom_value(Value::Int(1), 1, 12, 1, 12)])),
                    make_list(1, 15, 1, 19, Box::new([
                        make_atom("y", 1, 16, 1, 16),
                        make_atom_value(Value::Int(2), 1, 18, 1, 18)]))])),
                make_list(2, 5, 6, 10, Box::new([
                    make_atom("+", 2, 6, 2, 6),
                    make_atom("x", 2, 8, 2, 8),
                    make_list(4, 9, 5, 16, Box::new([
                        make_atom("let", 4, 10, 4, 12),
                        make_list(4, 14, 4, 20, Box::new([
                            make_list(4, 15, 4, 19, Box::new([
                                make_atom("x", 4, 16, 4, 16),
                                make_atom_value(Value::Int(3), 4, 18, 4, 18)]))])),
                        make_list(5, 9, 5, 15, Box::new([
                            make_atom("+", 5, 10, 5, 10),
                            make_atom("x", 5, 12, 5, 12),
                            make_atom("y", 5, 14, 5, 14)]))])),
                    make_atom("x", 6, 9, 6, 9)]))])),
            make_atom("x", 6, 13, 6, 13),
            make_atom("y", 6, 15, 6, 15),
        ];

        let parsed = ast::parser::parse(&input);
        assert_eq!(Ok(program), parsed, "Should match expected symbolic expression");

        let input = "        -1234
        (- 12 34)";
        let program = vec![ make_atom_value(Value::Int(-1234), 1, 9, 1, 13),
                            make_list(2, 9, 2, 17, Box::new([
                                make_atom("-", 2, 10,  2, 10),
                                make_atom_value(Value::Int(12), 2, 12, 2, 13),
                                make_atom_value(Value::Int(34), 2, 15, 2, 16)])) ];

        let parsed = ast::parser::parse(&input);
        assert_eq!(Ok(program), parsed, "Should match expected symbolic expression");

    }

    #[test]
    fn test_parse_tuple_literal () {
      let input = "{id 1337}";
      let program = vec![ make_tuple(1, 1, 1, 9, Box::new([
                              make_atom("id", 1, 2, 1, 3),
                              make_atom_value(Value::Int(1337), 1, 5, 1, 8)]))];
      let parsed = ast::parser::parse(&input);
      assert_eq!(Ok(program), parsed, "Should match expected tuple literal");
    }

    #[test]
    fn test_parse_contract_principals() {
        let input = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR.contract-a";
        let parsed = ast::parser::parse(&input).unwrap();

        let x1 = &parsed[0];
        assert!( match x1.match_atom_value() {
            Some(Value::Principal(PrincipalData::Contract(identifier))) => {
                format!("{}",
                    PrincipalData::Standard(identifier.issuer.clone())) == "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR" &&
                    identifier.name == "contract-a".into()
            },
            _ => false
        });
    }

    #[test]
    fn test_parse_generics() {
        let input = "<a>";
        let parsed = ast::parser::parse(&input).unwrap();

        let x1 = &parsed[0];
        assert!( match x1.match_trait_reference() {
            Some(trait_name) => *trait_name == "a".into(),
            _ => false
        });
    }

    #[test]
    fn test_parse_field_identifiers() {
        use vm::types::PrincipalData;
        let input = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR.my-contract.my-trait";
        let parsed = ast::parser::parse(&input).unwrap();

        let x1 = &parsed[0];
        assert!( match x1.match_field_identifier() {
            Some(data) => {
                format!("{}", PrincipalData::Standard(data.contract_identifier.issuer.clone())) == "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR" &&
                    data.contract_identifier.name == "my-contract".into() &&
                    data.name == "my-trait".into()
            },
            _ => false
        });
    }

    #[test]
    fn test_parse_sugared_field_identifiers() {
        let input = ".my-contract.my-trait";
        let parsed = ast::parser::parse(&input).unwrap();

        let x1 = &parsed[0];
        assert!( match &x1.pre_expr {
            PreSymbolicExpressionType::SugaredFieldIdentifier(contract_name, field_name) => {
                *contract_name == "my-contract".into() && *field_name == "my-trait".into()
            },
            _ => false
        });
    }

    #[test]
    fn test_parse_failures() {
        use vm::errors::{Error, RuntimeErrorType};

        let too_much_closure = "(let ((x 1) (y 2))))";
        let not_enough_closure = "(let ((x 1) (y 2))";
        let middle_hash = "(let ((x 1) (y#not 2)) x)";
        let unicode = "(let ((x🎶 1)) (eq x🎶 1))";
        let split_tokens = "(let ((023ab13 1)))";
        let name_with_dot = "(let ((ab.de 1)))";
        let wrong_tuple_literal_close = "{id 1337)";
        let wrong_list_close = "(13 37}";
        let extra_tuple_literal_close = "{37}}";
        let unexpected_comma = "(let ((a 1),(b 2)) b)";
        let shorthand_tuple = "{ a, b }";
        let shorthand_tuple_dangling_comma = "{ a: b, b: ,}";
        let decorative_colon_on_value = "{ a: b: }";
        let tuple_literal_colon_after_comma = "{ a: b, :b a}";
        let empty_tuple_literal_comma = "{,}";
        let empty_tuple_literal_colon = "{:}";
        let function_with_CR = "(define (foo (x y)) \n (+ 1 2 3) \r (- 1 2 3))";
        let function_with_CRLF = "(define (foo (x y)) \n (+ 1 2 3) \n\r (- 1 2 3))";
        let function_with_NEL = "(define (foo (x y)) \u{0085} (+ 1 2 3) \u{0085} (- 1 2 3))";
        let function_with_LS = "(define (foo (x y)) \u{2028} (+ 1 2 3) \u{2028} (- 1 2 3))";
        let function_with_PS = "(define (foo (x y)) \u{2029} (+ 1 2 3) \u{2029} (- 1 2 3))";
        // good case
        let function_with_LF = "(define (foo (x y)) \n (+ 1 2 3) \n (- 1 2 3))";

        assert!(match ast::parser::parse(&split_tokens).unwrap_err().err {
            ParseErrors::SeparatorExpected(_) => true, _ => false });

        assert!(match ast::parser::parse(&too_much_closure).unwrap_err().err {
            ParseErrors::ClosingParenthesisUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&not_enough_closure).unwrap_err().err {
            ParseErrors::ClosingParenthesisExpected => true, _ => false });

        assert!(match ast::parser::parse(&middle_hash).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });

        assert!(match ast::parser::parse(&unicode).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });

        assert!(match ast::parser::parse(&name_with_dot).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });

        assert!(match ast::parser::parse(&wrong_tuple_literal_close).unwrap_err().err {
            ParseErrors::ClosingTupleLiteralExpected => true, _ => false });

        assert!(match ast::parser::parse(&wrong_list_close).unwrap_err().err {
            ParseErrors::ClosingParenthesisExpected => true, _ => false });

        assert!(match ast::parser::parse(&extra_tuple_literal_close).unwrap_err().err {
            ParseErrors::ClosingTupleLiteralUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&unexpected_comma).unwrap_err().err {
            ParseErrors::CommaSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&shorthand_tuple).unwrap_err().err {
            ParseErrors::CommaSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&shorthand_tuple_dangling_comma).unwrap_err().err {
            ParseErrors::CommaSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&decorative_colon_on_value).unwrap_err().err {
            ParseErrors::ColonSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&tuple_literal_colon_after_comma).unwrap_err().err {
            ParseErrors::ColonSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&empty_tuple_literal_comma).unwrap_err().err {
            ParseErrors::CommaSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&empty_tuple_literal_colon).unwrap_err().err {
            ParseErrors::ColonSeparatorUnexpected => true, _ => false });

        assert!(match ast::parser::parse(&function_with_CR).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });
        assert!(match ast::parser::parse(&function_with_CRLF).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });
        assert!(match ast::parser::parse(&function_with_NEL).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });
        assert!(match ast::parser::parse(&function_with_LS).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });
        assert!(match ast::parser::parse(&function_with_PS).unwrap_err().err {
            ParseErrors::FailedParsingRemainder(_) => true, _ => false });

        ast::parser::parse(&function_with_LF).unwrap();
    }

}
