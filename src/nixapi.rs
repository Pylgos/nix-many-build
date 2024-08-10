use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use anyhow::{ensure, Result};
use nom::{
    bytes::complete::tag, character::complete::char, combinator::opt, error::Error, multi::many0,
    sequence::terminated, IResult, Parser,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

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
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DrvPath(pub String);

impl DrvPath {
    pub fn get_name(&self) -> &str {
        let filename = self.0.rsplit_once('/').unwrap().1;
        &filename[33..filename.len() - 4]
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
    if Path::new(&path.0).exists() {
        return CacheStatus::Local;
    }
    CacheStatus::NotBuilt
}

pub async fn query_requisites(path: &StorePath) -> Result<Vec<StorePath>> {
    let output = Command::new("nix-store")
        .kill_on_drop(true)
        .arg("--query")
        .arg("--requisites")
        .arg(&path.0)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "failed to query requisites of {}",
        path.0
    );
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout
        .lines()
        .map(|line| StorePath(line.to_string()))
        .collect())
}

#[derive(Debug, Clone)]
pub struct Derivation {
    pub path: DrvPath,
    pub inputs: Vec<(DrvPath, Vec<OutputName>)>,
    pub outputs: BTreeMap<OutputName, StorePath>,
    pub env: HashMap<String, String>,
    pub attr: Option<String>,
}

impl Derivation {
    pub fn parse(drv_path: DrvPath) -> Result<Self> {
        let content = std::fs::read_to_string(&drv_path.0)?;
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
            attr: None,
        })
    }

    pub fn is_source(&self) -> bool {
        self.env.contains_key("outputHash")
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
    #[test]
    fn test_parse_derivation() {
        let content = r#"Derive([("debug","/nix/store/3z5d3hrjbjqpp7fgb05vby5xswdz5x3y-zstd_point_cloud_transport-4.0.0-1-debug","",""),("out","/nix/store/zg4yjfj0q8q3spwf47zn4ash9c1pb2nx-zstd_point_cloud_transport-4.0.0-1","","")],[("/nix
/store/0my8p06bzqzna5xzmp3idgh0xy6b2nh8-python3.11-colcon-common-extensions-0.3.0.drv",["out"]),("/nix/store/63366n7x14rn0k2isf58nsqk7vjf21vs-ament_cmake-2.5.2-1.drv",["out"]),("/nix/store/b5bgmmj09jflc1dvnb1wgabk3
w863c7y-pluginlib-5.4.2-2.drv",["out"]),("/nix/store/cxsx9m8fm6n1fwbdi594rlvqmaqp7dch-stdenv-linux.drv",["out"]),("/nix/store/fvfnmazgg0mclma9hvc6rwb91clb7izk-zstd_point_cloud_transport-source.drv",["out"]),("/nix/
store/iaf8nhl6pn8rxlbw6p31pjh2pglj9jql-point_cloud_transport-4.0.2-1.drv",["out"]),("/nix/store/j5y5s2h3x12xdr25kd164bnigjpr1939-ros2nix-setup-hook.drv",["out"]),("/nix/store/k7k3vjsl4fiy5z2675ihlh3hs3b7fp85-point_
cloud_interfaces-4.0.0-1.drv",["out"]),("/nix/store/m6fbf3gbgz227iihn7yj809qclcl69wj-zstd-1.5.6.drv",["dev"]),("/nix/store/qkzvk6rz1dgrspwyqzq5crmxghc99bbq-rclcpp-28.1.3-1.drv",["out"]),("/nix/store/rr9xr8km0aanlf2
hadsczy29ikcbx500-bash-5.2p26.drv",["out"])],["/nix/store/12l2v3kmacnpmx14p2345kk41fpv31rw-separate-debug-info.sh","/nix/store/v6x3cs394jgqfbi0a42pam708flxaphh-default-builder.sh"],"x86_64-linux","/nix/store/306zny
j77fv49kwnkpxmb0j2znqpa8bj-bash-5.2p26/bin/bash",["-e","/nix/store/v6x3cs394jgqfbi0a42pam708flxaphh-default-builder.sh"],[("__structuredAttrs",""),("buildInputs",""),("buildPhase","runHook preBuild\n\nMAKEFLAGS=\"-
j$NIX_BUILD_CORES\" colcon --log-base ./log build \\\n  --paths . \\\n  --merge-install \\\n  --install-base \"$out\" \\\n  --build-base ./build \\\n  --event-handlers console_cohesion- console_direct+ console_pack
age_list- console_start_end- console_stderr- desktop_notification- event_log- log- log_command- status- store_result- summary- terminal_title- \\\n  --executor sequential --cmake-args ' -DCMAKE_BUILD_TYPE=RelWithDe
bInfo' --cmake-args ' -DBUILD_TESTING=OFF'\n\nrunHook postBuild\n"),("builder","/nix/store/306znyj77fv49kwnkpxmb0j2znqpa8bj-bash-5.2p26/bin/bash"),("checkPhase","runHook preCheck\n\ncolcon test \\\n  --merge-instal
l \\\n  --install-base $out\n\nrunHook postCheck\n"),("cmakeFlags",""),("configureFlags",""),("debug","/nix/store/3z5d3hrjbjqpp7fgb05vby5xswdz5x3y-zstd_point_cloud_transport-4.0.0-1-debug"),("depsBuildBuild",""),("
depsBuildBuildPropagated",""),("depsBuildTarget",""),("depsBuildTargetPropagated",""),("depsHostHost",""),("depsHostHostPropagated",""),("depsTargetTarget",""),("depsTargetTargetPropagated",""),("doCheck",""),("doI
nstallCheck",""),("dontUseCmakeConfigure","1"),("dontWrapQtApps","1"),("installPhase","runHook preInstall\nrunHook postInstall\n"),("mesonFlags",""),("name","zstd_point_cloud_transport-4.0.0-1"),("nativeBuildInputs
","/nix/store/8arxm3iw11bw26i7bm3p81jakn7b3pig-ament_cmake-2.5.2-1 /nix/store/ygimj20l4z9ykg6s7smndrgn8v1k25n4-python3.11-colcon-common-extensions-0.3.0 /nix/store/x9wyqwhhzivgg0ry88gj11ywb7m5zb1p-ros2nix-setup-hoo
k /nix/store/12l2v3kmacnpmx14p2345kk41fpv31rw-separate-debug-info.sh"),("out","/nix/store/zg4yjfj0q8q3spwf47zn4ash9c1pb2nx-zstd_point_cloud_transport-4.0.0-1"),("outputs","out debug"),("patches",""),("pname","zstd_
point_cloud_transport"),("propagatedBuildInputs","/nix/store/zzsq5v9qkfnznc1zbb89rykx0s84m6vr-pluginlib-5.4.2-2 /nix/store/z9c56kfjmbbbnqgfrazg2jj2lvrswd56-point_cloud_interfaces-4.0.0-1 /nix/store/c6snhfji9iaphfdk
bd0yhrbzqj0pjr3d-point_cloud_transport-4.0.2-1 /nix/store/gim11j9jpwhyn9b148ab8jr01fb8ch8f-rclcpp-28.1.3-1 /nix/store/9ynakmwsf7nqhlv3phl05yybc5ad754l-zstd-1.5.6-dev"),("propagatedNativeBuildInputs",""),("separateD
ebugInfo","1"),("shellHook","ROS2NIX_SETUP_DEVEL_ENV=1 _ros2nixSetupHook_postHook 2> /dev/null\n"),("src","/nix/store/ip1iplr6qqpyi8i7bm6y8jw3vlvvw3dd-zstd_point_cloud_transport-source"),("stdenv","/nix/store/xfhkj
npqjwlf6hlk1ysmq3aaq80f3bjj-stdenv-linux"),("strictDeps","1"),("system","x86_64-linux"),("version","4.0.0-1")])"#;
        parse_derivation(content).unwrap();
    }
}
