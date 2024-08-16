use std::{
    collections::{BTreeMap, HashMap},
    ops::Deref,
    path::Path,
};

use anyhow::Result;
use nom::{
    bytes::complete::tag, character::complete::char, combinator::opt, error::Error, multi::many0,
    sequence::terminated, IResult, Parser,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StorePath(pub String);

impl StorePath {
    pub fn hash_part(&self) -> &str {
        &self.0.rsplit_once('/').unwrap().1[..32]
    }

    pub fn get_name(&self) -> &str {
        &self.0.rsplit_once('/').unwrap().1[33..]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for StorePath {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl AsRef<Path> for StorePath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DrvPath(pub String);

impl DrvPath {
    pub fn get_name(&self) -> &str {
        let filename = self.0.rsplit_once('/').unwrap().1;
        &filename[33..filename.len() - 4]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for DrvPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl AsRef<Path> for DrvPath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputName(pub String);

#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum CacheStatus {
    NotBuilt,
    Local,
    Cached,
}

pub async fn check_cache_status(
    client: &reqwest::Client,
    path: &StorePath,
    substituters: &[Url],
) -> CacheStatus {
    for substituter in substituters {
        let narinfo_filename = format!("{}.narinfo", path.hash_part());
        let url = substituter.join(&narinfo_filename).unwrap();
        let resp = client.head(url.clone()).send().await;
        match resp {
            Err(_) => continue,
            Ok(resp) => {
                if resp.status().is_success() {
                    return CacheStatus::Cached;
                }
            }
        }
    }
    if path.exists() {
        return CacheStatus::Local;
    }
    CacheStatus::NotBuilt
}

#[derive(Debug, Clone)]
pub struct Derivation {
    pub path: DrvPath,
    pub inputs: Vec<(DrvPath, Vec<OutputName>)>,
    pub outputs: BTreeMap<OutputName, StorePath>,
    pub env: HashMap<String, String>,
}

impl Derivation {
    pub fn parse(drv_path: DrvPath) -> Result<Self> {
        let content = std::fs::read_to_string(&drv_path)?;
        let (_, res) = parse_derivation(&content).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse derivation file {drv_path:?}: {error}",
                drv_path = drv_path.0,
                error = e
            )
        })?;
        Ok(Derivation {
            path: drv_path.clone(),
            inputs: res.inputs,
            outputs: res.outputs.into_iter().collect(),
            env: res.env.into_iter().collect(),
        })
    }

    pub fn is_local(&self) -> bool {
        self.env.get("preferLocalBuild").map(|s| s.as_str()) == Some("1")
    }
}

fn parse_string(input: &str) -> IResult<&str, String> {
    let (input, _) = char('"')(input)?;
    let mut chars = input.char_indices();
    let mut result = String::new();
    loop {
        let (i, c) = chars
            .next()
            .ok_or_else(|| nom::Err::Error(Error::new(input, nom::error::ErrorKind::Eof)))?;
        match c {
            '"' => return Ok((&input[i + 1..], result)),
            '\\' => {
                let (_, c) = chars.next().ok_or_else(|| {
                    nom::Err::Error(Error::new(input, nom::error::ErrorKind::Eof))
                })?;
                match c {
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    _ => {
                        return Err(nom::Err::Error(Error::new(
                            input,
                            nom::error::ErrorKind::Char,
                        )))
                    }
                };
            }
            _ => result.push(c),
        };
    }
}

fn parse_output(input: &str) -> IResult<&str, (OutputName, StorePath)> {
    let (input, _) = char('(')(input)?;
    let (input, name) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, path) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _hash_algo) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _hash) = parse_string(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, (OutputName(name), StorePath(path))))
}

fn parse_drv_input(input: &str) -> IResult<&str, (DrvPath, Vec<OutputName>)> {
    let (input, _) = char('(')(input)?;
    let (input, path) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, outputs) = parse_list(parse_string)(input)?;
    let (input, _) = char(')')(input)?;
    Ok((
        input,
        (DrvPath(path), outputs.into_iter().map(OutputName).collect()),
    ))
}

fn parse_env(input: &str) -> IResult<&str, (String, String)> {
    let (input, _) = char('(')(input)?;
    let (input, name) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, value) = parse_string(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, (name, value)))
}

fn parse_list<'a, F, O>(mut parser: F) -> impl FnMut(&'a str) -> IResult<&str, Vec<O>>
where
    F: Parser<&'a str, O, Error<&'a str>>,
{
    move |input: &'a str| {
        let (input, _) = char('[')(input)?;
        let (input, result) = many0(terminated(ParserRef(&mut parser), opt(char(','))))(input)?;
        let (input, _) = char(']')(input)?;
        Ok((input, result))
    }
}

struct ParseDerivationResult {
    outputs: Vec<(OutputName, StorePath)>,
    inputs: Vec<(DrvPath, Vec<OutputName>)>,
    env: Vec<(String, String)>,
}

fn parse_derivation(input: &str) -> IResult<&str, ParseDerivationResult> {
    let (input, _) = tag("Derive(")(input)?;
    let (input, outputs) = parse_list(parse_output)(input)?;
    let (input, _) = char(',')(input)?;
    let (input, drv_inputs) = parse_list(parse_drv_input)(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _path_inputs) = parse_list(parse_string)(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _system) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _builder) = parse_string(input)?;
    let (input, _) = char(',')(input)?;
    let (input, _builder_args) = parse_list(parse_string)(input)?;
    let (input, _) = char(',')(input)?;
    let (input, env) = parse_list(parse_env)(input)?;
    let (input, _) = tag(")")(input)?;
    Ok((
        input,
        ParseDerivationResult {
            outputs,
            inputs: drv_inputs,
            env,
        },
    ))
}

struct ParserRef<'a, P>(&'a mut P);
impl<'a, I, O, E, P: Parser<I, O, E>> Parser<I, O, E> for ParserRef<'a, P> {
    fn parse(&mut self, input: I) -> nom::IResult<I, O, E> {
        self.0.parse(input)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_parse_string() {
        assert_eq!(parse_string(r#""foo""#), Ok(("", "foo".to_string())));
        assert_eq!(
            parse_string(r#""foo\"bar""#),
            Ok(("", r#"foo"bar"#.to_string()))
        );
        assert_eq!(
            parse_string(r#""foo\nbar""#),
            Ok(("", "foo\nbar".to_string()))
        );
        assert_eq!(
            parse_string(r#""foo\rbar""#),
            Ok(("", "foo\rbar".to_string()))
        );
        assert_eq!(
            parse_string(r#""foo\tbar""#),
            Ok(("", "foo\tbar".to_string()))
        );
    }
    #[test]
    fn test_parse_output() {
        assert_eq!(
            parse_output(r#"("name","path","hash_algo","hash")"#),
            Ok((
                "",
                (
                    OutputName("name".to_string()),
                    StorePath("path".to_string())
                )
            ))
        );
    }
    #[test]
    fn test_parse_drv_input() {
        assert_eq!(
            parse_drv_input(r#"("path",["output1","output2"])"#),
            Ok((
                "",
                (
                    DrvPath("path".to_string()),
                    vec![
                        OutputName("output1".to_string()),
                        OutputName("output2".to_string())
                    ]
                )
            ))
        );
    }
    #[test]
    fn test_parse_env() {
        assert_eq!(
            parse_env(r#"("name","value")"#),
            Ok(("", ("name".to_string(), "value".to_string())))
        );
    }
    #[test]
    fn test_parse_list() {
        assert_eq!(
            parse_list(parse_string)(r#"["foo","bar"]"#),
            Ok(("", vec!["foo".to_string(), "bar".to_string()]))
        );
    }
}
