//! Provide utilities for evaluating job specification directives.
//!
//! The job specification directives for `cargo-maelstrom` and the CLI differ in a number of ways, but
//! also have a number of similar constructs. This module includes utilities for those similar
//! constructs.

pub mod substitute;

use crate::{proto, IntoProtoBuf, TryFromProtoBuf};
use anyhow::{anyhow, Error, Result};
use enumset::{EnumSet, EnumSetType};
use maelstrom_base::Utf8PathBuf;
use maelstrom_util::template::{replace_template_vars, TemplateVars};
use serde::{de, Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env::{self, VarError},
    path::PathBuf,
    result,
};
use tuple::Map as _;

/// A function that can passed to [`substitute::substitute`] as the `env_lookup` closure that will
/// resolve variables from the program's environment.
pub fn std_env_lookup(var: &str) -> Result<Option<String>> {
    match env::var(var) {
        Ok(val) => Ok(Some(val)),
        Err(VarError::NotPresent) => Ok(None),
        Err(err) => Err(Error::new(err)),
    }
}

/// A function used when writing customer deserializers for job specification directives to
/// indicate that two fields are incompatible.
pub fn incompatible<T, E>(field: &Option<T>, msg: &str) -> result::Result<(), E>
where
    E: de::Error,
{
    if field.is_some() {
        Err(E::custom(format_args!("{}", msg)))
    } else {
        Ok(())
    }
}

#[derive(
    IntoProtoBuf,
    TryFromProtoBuf,
    Clone,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    Serialize,
)]
#[proto(other_type = "proto::PrefixOptions")]
pub struct PrefixOptions {
    pub strip_prefix: Option<Utf8PathBuf>,
    pub prepend_prefix: Option<Utf8PathBuf>,
    #[serde(default)]
    pub canonicalize: bool,
    #[serde(default)]
    pub follow_symlinks: bool,
}

#[derive(
    IntoProtoBuf,
    TryFromProtoBuf,
    Clone,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    Serialize,
)]
#[proto(other_type = "proto::SymlinkSpec")]
pub struct SymlinkSpec {
    pub link: Utf8PathBuf,
    pub target: Utf8PathBuf,
}

#[derive(
    IntoProtoBuf, TryFromProtoBuf, Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize,
)]
#[proto(other_type = proto::add_layer_request::Layer)]
#[serde(untagged, deny_unknown_fields)]
pub enum Layer {
    #[proto(other_type = proto::TarLayer)]
    Tar {
        #[serde(rename = "tar")]
        path: Utf8PathBuf,
    },
    #[proto(other_type = proto::GlobLayer)]
    Glob {
        glob: String,
        #[serde(flatten)]
        #[proto(option)]
        prefix_options: PrefixOptions,
    },
    #[proto(other_type = proto::PathsLayer)]
    Paths {
        paths: Vec<Utf8PathBuf>,
        #[serde(flatten)]
        #[proto(option)]
        prefix_options: PrefixOptions,
    },
    #[proto(other_type = proto::StubsLayer)]
    Stubs { stubs: Vec<String> },
    #[proto(other_type = proto::SymlinksLayer)]
    Symlinks { symlinks: Vec<SymlinkSpec> },
}

impl Layer {
    pub fn replace_template_vars(&mut self, vars: &TemplateVars) -> Result<()> {
        match self {
            Self::Tar { path } => *path = replace_template_vars(path.as_str(), vars)?.into(),
            Self::Glob { glob, .. } => *glob = replace_template_vars(glob, vars)?,
            Self::Paths { paths, .. } => {
                for path in paths {
                    *path = replace_template_vars(path.as_str(), vars)?.into();
                }
            }
            Self::Stubs { stubs, .. } => {
                for stub in stubs {
                    *stub = replace_template_vars(stub, vars)?;
                }
            }
            Self::Symlinks { symlinks } => {
                for SymlinkSpec { link, target } in symlinks {
                    *link = replace_template_vars(link.as_str(), vars)?.into();
                    *target = replace_template_vars(target.as_str(), vars)?.into();
                }
            }
        }
        Ok(())
    }
}

/// An enum and struct (`EnumSet<ImageUse>`) used for deserializing "image use" statements in JSON,
/// TOML, or other similar formats. This allows users to specify things like
/// `use = ["layers", "environment"]` in TOML, or the equivalent in JSON.
///
/// See [`Image`].
#[derive(Debug, Deserialize, EnumSetType, Serialize)]
#[serde(rename_all = "snake_case")]
#[enumset(serialize_repr = "list")]
pub enum ImageUse {
    Layers,
    Environment,
    WorkingDirectory,
}

/// A struct used for deserializing "image" statements in JSON, TOML, or other similar formats.
/// This allows the user to specify an image name and the parts of the image they want to use.
#[derive(Deserialize)]
pub struct Image {
    pub name: String,
    #[serde(rename = "use")]
    pub use_: EnumSet<ImageUse>,
}

/// A simple wrapper struct for the config of a local OCI image. This is used for dependency
/// injection for the other functions in this module.
#[derive(Default)]
pub struct ImageConfig {
    /// Local `PathBuf`s pointing to the various layer artifacts.
    pub layers: Vec<PathBuf>,

    /// Optional `PathBuf` in the container's namespace for the working directory.
    pub working_directory: Option<Utf8PathBuf>,

    /// Optional environment variables for the container, assumed to be in `VAR=value` format.
    pub environment: Option<Vec<String>>,
}

/// An enum that indicates whether a value is explicitly specified, or implicitly defined to be the
/// value inherited from an image.
#[derive(PartialEq, Eq, Debug, Deserialize)]
pub enum PossiblyImage<T> {
    /// The value comes from the corresponding value in the image.
    Image,

    /// The value is explicitly set, and doesn't come from the image.
    Explicit(T),
}

/// A convenience struct for extracting parts of an OCI image for use in a
/// [`maelstrom_base::JobSpec`].
pub struct ImageOption<'a> {
    name: Option<&'a str>,
    layers: Vec<PathBuf>,
    environment: Option<Vec<String>>,
    working_directory: Option<Utf8PathBuf>,
}

impl<'a> ImageOption<'a> {
    /// Create a new [`ImageOption`].
    pub fn new(
        image_name: &'a Option<String>,
        image_lookup: impl FnMut(&str) -> Result<ImageConfig>,
    ) -> Result<Self> {
        let name = image_name.as_deref();
        let (layers, environment, working_directory) =
            image_name.as_deref().map(image_lookup).transpose()?.map_or(
                (Default::default(), Default::default(), Default::default()),
                |ImageConfig {
                     layers,
                     environment,
                     working_directory,
                 }| { (layers, environment, working_directory) },
            );
        Ok(ImageOption {
            name,
            layers,
            environment,
            working_directory,
        })
    }

    /// Return the image name. A non-`None` image name must have been specified when this struct
    /// was created, or this function will panic.
    pub fn name(&self) -> &str {
        self.name
            .expect("name() called on an ImageOption that has no image name")
    }

    /// Return an iterator of layers for the image. If there is no image, the iterator will be
    /// empty.
    pub fn layers(&self) -> Result<impl Iterator<Item = Layer>> {
        Ok(self
            .layers
            .iter()
            .map(|p| {
                Ok(Layer::Tar {
                    path: Utf8PathBuf::from_path_buf(p.to_owned()).map_err(|_| {
                        anyhow!("image {} has a non-UTF-8 layer path {p:?}", self.name())
                    })?,
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter())
    }

    /// Return a [`BTreeMap`] of environment variables for the image. If the image doesn't have any
    /// environment variables, this will return an error.
    pub fn environment(&self) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::from_iter(
            self.environment
                .as_ref()
                .ok_or_else(|| anyhow!("image {} has no environment to use", self.name()))?
                .iter()
                .map(|var| {
                    var.split_once('=')
                        .map(|pair| pair.map(str::to_string))
                        .ok_or_else(|| {
                            anyhow!(
                                "image {} has an invalid environment variable {var}",
                                self.name(),
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
        ))
    }

    /// Return the working directory for the image. If the image doesn't have a working directory,
    /// this will return an error.
    pub fn working_directory(&self) -> Result<Utf8PathBuf> {
        self.working_directory
            .clone()
            .ok_or_else(|| anyhow!("image {} has no working directory to use", self.name()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use maelstrom_test::{path_buf_vec, string, string_vec, tar_layer};
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

    #[test]
    fn std_env_lookup_good() {
        let var = "AN_ENVIRONMENT_VARIABLE_1";
        let val = "foobar";
        env::set_var(var, val);
        assert_eq!(std_env_lookup(var).unwrap(), Some(val.to_string()));
    }

    #[test]
    fn std_env_lookup_missing() {
        let var = "AN_ENVIRONMENT_VARIABLE_TO_DELETE";
        env::remove_var(var);
        assert_eq!(std_env_lookup(var).unwrap(), None);
    }

    #[test]
    fn std_env_lookup_error() {
        let var = "AN_ENVIRONMENT_VARIABLE_2";
        let val = unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(vec![0xff]) };
        env::set_var(var, val);
        assert_eq!(
            format!("{}", std_env_lookup(var).unwrap_err()),
            r#"environment variable was not valid unicode: "\xFF""#
        );
    }

    fn images(name: &str) -> Result<ImageConfig> {
        match name {
            "image1" => Ok(ImageConfig {
                layers: path_buf_vec!["42", "43"],
                working_directory: Some("/foo".into()),
                environment: Some(string_vec!["FOO=image-foo", "BAZ=image-baz",]),
            }),
            "empty" => Ok(Default::default()),
            "invalid-env" => Ok(ImageConfig {
                environment: Some(string_vec!["FOO"]),
                ..Default::default()
            }),
            "invalid-layer-path" => Ok(ImageConfig {
                layers: vec![PathBuf::from(OsStr::from_bytes(b"\xff"))],
                ..Default::default()
            }),
            _ => Err(anyhow!("no container named {name} found")),
        }
    }

    fn assert_error(err: anyhow::Error, expected: &str) {
        let message = format!("{err}");
        assert!(
            message == expected,
            "message: {message:?}, expected: {expected:?}"
        );
    }

    #[test]
    fn good_image_option() {
        let image_name = Some(string!("image1"));
        let io = ImageOption::new(&image_name, images).unwrap();
        assert_eq!(io.name(), "image1");
        assert_eq!(
            Vec::from_iter(io.layers().unwrap()),
            vec![tar_layer!("42"), tar_layer!("43")],
        );
        assert_eq!(
            io.environment().unwrap(),
            BTreeMap::from([
                (string!("BAZ"), string!("image-baz")),
                (string!("FOO"), string!("image-foo")),
            ]),
        );
        assert_eq!(io.working_directory().unwrap(), PathBuf::from("/foo"));
    }

    #[test]
    fn image_option_no_environment_and_no_working_directory() {
        let image_name = Some(string!("empty"));
        let io = ImageOption::new(&image_name, images).unwrap();
        assert_error(
            io.environment().unwrap_err(),
            "image empty has no environment to use",
        );
        assert_error(
            io.working_directory().unwrap_err(),
            "image empty has no working directory to use",
        );
    }

    #[test]
    fn image_option_invalid_environment_variable() {
        let image_name = Some(string!("invalid-env"));
        let io = ImageOption::new(&image_name, images).unwrap();
        assert_error(
            io.environment().unwrap_err(),
            "image invalid-env has an invalid environment variable FOO",
        );
    }

    #[test]
    fn image_option_invalid_layer_path() {
        let image_name = Some(string!("invalid-layer-path"));
        let io = ImageOption::new(&image_name, images).unwrap();
        let Err(err) = io.layers() else {
            panic!("");
        };
        assert_error(
            err,
            r#"image invalid-layer-path has a non-UTF-8 layer path "\xFF""#,
        );
    }
}
