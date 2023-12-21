use crate::substitute;
use anyhow::{anyhow, Result};
use enumset::EnumSetType;
use meticulous_base::{
    EnumSet, GroupId, JobDevice, JobDeviceListDeserialize, JobMount, JobSpec, NonEmpty,
    Sha256Digest, UserId,
};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

struct JobSpecIterator<InnerT, LayerMapperT, EnvLookupT, ImageLookupT> {
    inner: InnerT,
    layer_mapper: LayerMapperT,
    env_lookup: EnvLookupT,
    image_lookup: ImageLookupT,
}

impl<InnerT, LayerMapperT, EnvLookupT, ImageLookupT> Iterator
    for JobSpecIterator<InnerT, LayerMapperT, EnvLookupT, ImageLookupT>
where
    InnerT: Iterator<Item = serde_json::Result<Job>>,
    LayerMapperT: Fn(String) -> anyhow::Result<NonEmpty<Sha256Digest>>,
    EnvLookupT: Fn(&str) -> Result<Option<String>>,
    ImageLookupT: FnMut(&str) -> Result<ContainerImage>,
{
    type Item = anyhow::Result<JobSpec>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            None => None,
            Some(Err(err)) => Some(Err(anyhow::Error::new(err))),
            Some(Ok(job)) => Some(job.into_job_spec(
                &self.layer_mapper,
                &self.env_lookup,
                &mut self.image_lookup,
            )),
        }
    }
}

pub fn job_spec_iter_from_reader(
    reader: impl Read,
    layer_mapper: impl Fn(String) -> anyhow::Result<NonEmpty<Sha256Digest>>,
    env_lookup: impl Fn(&str) -> Result<Option<String>>,
    image_lookup: impl FnMut(&str) -> Result<ContainerImage>,
) -> impl Iterator<Item = anyhow::Result<JobSpec>> {
    let inner = serde_json::Deserializer::from_reader(reader).into_iter::<Job>();
    JobSpecIterator {
        inner,
        layer_mapper,
        env_lookup,
        image_lookup,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Job {
    program: String,
    arguments: Option<Vec<String>>,
    environment: Option<PossiblyImage<BTreeMap<String, String>>>,
    added_environment: BTreeMap<String, String>,
    layers: PossiblyImage<NonEmpty<String>>,
    added_layers: Vec<String>,
    devices: Option<EnumSet<JobDeviceListDeserialize>>,
    mounts: Option<Vec<JobMount>>,
    enable_loopback: Option<bool>,
    enable_writable_file_system: Option<bool>,
    working_directory: Option<PossiblyImage<PathBuf>>,
    user: Option<UserId>,
    group: Option<GroupId>,
    image: Option<String>,
}

impl Job {
    #[cfg(test)]
    fn new(program: String, layers: NonEmpty<String>) -> Self {
        Job {
            program,
            layers: PossiblyImage::Explicit(layers),
            added_layers: Default::default(),
            arguments: None,
            environment: None,
            added_environment: Default::default(),
            devices: None,
            mounts: None,
            enable_loopback: None,
            enable_writable_file_system: None,
            working_directory: None,
            user: None,
            group: None,
            image: None,
        }
    }

    fn into_job_spec(
        self,
        layer_mapper: impl Fn(String) -> anyhow::Result<NonEmpty<Sha256Digest>>,
        env_lookup: impl Fn(&str) -> Result<Option<String>>,
        image_lookup: impl FnMut(&str) -> Result<ContainerImage>,
    ) -> anyhow::Result<JobSpec> {
        let (image_layers, image_environment, image_working_directory) =
            self.image.as_deref().map(image_lookup).transpose()?.map_or(
                (None, None, None),
                |ContainerImage {
                     layers,
                     environment,
                     working_directory,
                 }| { (Some(layers), Some(environment), Some(working_directory)) },
            );
        let image_name = self.image.as_deref().unwrap_or("");
        let mut environment = match self.environment {
            None => BTreeMap::default(),
            Some(PossiblyImage::Explicit(environment)) => environment
                .into_iter()
                .map(|(k, v)| -> Result<_> {
                    Ok((
                        k,
                        substitute::substitute(&v, &env_lookup, |_| Option::<String>::None)?
                            .into_owned(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
            Some(PossiblyImage::Image) => {
                let image_environment = image_environment
                    .unwrap()
                    .ok_or_else(|| anyhow!("image {image_name} has no environment to use"))?;
                let mut environment = BTreeMap::default();
                for var in image_environment {
                    match var.split_once('=') {
                        None => {
                            return Err(anyhow!(
                                "image {image_name} has an invalid environment variable {var}"
                            ));
                        }
                        Some((left, right)) => {
                            environment.insert(left.into(), right.into());
                        }
                    }
                }
                environment
            }
        };
        let added_environment = self
            .added_environment
            .into_iter()
            .map(|(k, v)| -> Result<_> {
                Ok((
                    k,
                    substitute::substitute(&v, &env_lookup, |var| {
                        environment.get(var).map(|v| v.as_str())
                    })?
                    .into_owned(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        environment.extend(added_environment);
        let environment = Vec::from_iter(environment.into_iter().map(|(k, v)| k + "=" + &v));
        let mut layers = match self.layers {
            PossiblyImage::Explicit(layers) => layers,
            PossiblyImage::Image => NonEmpty::from_vec(image_layers.unwrap())
                .ok_or_else(|| anyhow!("image {image_name} has no layers to use"))?
                .map(|pb| pb.into_os_string().into_string().unwrap()),
        };
        layers.extend(self.added_layers);
        let layers = NonEmpty::<Sha256Digest>::flatten(layers.try_map(layer_mapper)?);
        let working_directory = match self.working_directory {
            None => PathBuf::from("/"),
            Some(PossiblyImage::Explicit(working_directory)) => working_directory,
            Some(PossiblyImage::Image) => image_working_directory
                .unwrap()
                .ok_or_else(|| anyhow!("image {image_name} has no working_directory to use"))?,
        };
        Ok(JobSpec {
            program: self.program,
            arguments: self.arguments.unwrap_or_default(),
            environment,
            layers,
            devices: self
                .devices
                .unwrap_or(EnumSet::EMPTY)
                .into_iter()
                .map(JobDevice::from)
                .collect(),
            mounts: self.mounts.unwrap_or_default(),
            enable_loopback: self.enable_loopback.unwrap_or_default(),
            enable_writable_file_system: self.enable_writable_file_system.unwrap_or_default(),
            working_directory,
            user: self.user.unwrap_or(UserId::from(0)),
            group: self.group.unwrap_or(GroupId::from(0)),
        })
    }
}

#[derive(Default)]
pub struct ContainerImage {
    pub layers: Vec<PathBuf>,
    pub working_directory: Option<PathBuf>,
    pub environment: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum JobField {
    Program,
    Arguments,
    Environment,
    AddedEnvironment,
    Layers,
    AddedLayers,
    Devices,
    Mounts,
    EnableLoopback,
    EnableWritableFileSystem,
    WorkingDirectory,
    User,
    Group,
    Image,
}

struct JobVisitor;

#[derive(Debug, Deserialize, EnumSetType, Serialize)]
#[serde(rename_all = "snake_case")]
#[enumset(serialize_repr = "list")]
enum ImageUse {
    Layers,
    Environment,
    WorkingDirectory,
}

#[derive(Deserialize)]
struct DirectiveImage {
    name: String,
    #[serde(rename = "use")]
    use_: EnumSet<ImageUse>,
}

#[derive(PartialEq, Eq, Debug, Deserialize)]
pub enum PossiblyImage<T> {
    Image,
    Explicit(T),
}

impl<'de> de::Visitor<'de> for JobVisitor {
    type Value = Job;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Job")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        let mut program = None;
        let mut arguments = None;
        let mut environment = None;
        let mut added_environment = None;
        let mut layers = None;
        let mut added_layers = None;
        let mut devices = None;
        let mut mounts = None;
        let mut enable_loopback = None;
        let mut enable_writable_file_system = None;
        let mut working_directory = None;
        let mut user = None;
        let mut group = None;
        let mut image = None;
        while let Some(key) = map.next_key()? {
            match key {
                JobField::Program => {
                    program = Some(map.next_value()?);
                }
                JobField::Arguments => {
                    arguments = Some(map.next_value()?);
                }
                JobField::Environment => {
                    if environment.is_some() {
                        assert!(matches!(environment, Some(PossiblyImage::Image)));
                        return Err(de::Error::custom(format_args!(concat!(
                            "field `environment` cannot be set if `image` with a `use` of ",
                            "`environment` is also set (try `added_environment` instead)"
                        ))));
                    }
                    environment = Some(PossiblyImage::Explicit(map.next_value()?));
                }
                JobField::AddedEnvironment => match &environment {
                    None => {
                        return Err(de::Error::custom(format_args!(
                                        "field `added_environment` set before `image` with a `use` of `environment`"
                            )));
                    }
                    Some(PossiblyImage::Explicit(_)) => {
                        return Err(de::Error::custom(format_args!(
                            "field `added_environment` cannot be set with `environment` field"
                        )));
                    }
                    Some(PossiblyImage::Image) => {
                        added_environment = Some(map.next_value()?);
                    }
                },
                JobField::Layers => {
                    if layers.is_some() {
                        assert!(matches!(layers, Some(PossiblyImage::Image)));
                        return Err(de::Error::custom(format_args!(concat!(
                            "field `layers` cannot be set if `image` with a `use` of ",
                            "`layers` is also set (try `added_layers` instead)"
                        ))));
                    }
                    layers = Some(PossiblyImage::Explicit(
                        NonEmpty::from_vec(map.next_value()?).ok_or_else(|| {
                            de::Error::custom(format_args!("field `layers` cannot be empty"))
                        })?,
                    ));
                }
                JobField::AddedLayers => match &layers {
                    None => {
                        return Err(de::Error::custom(format_args!(
                            "field `added_layers` set before `image` with a `use` of `layers`"
                        )));
                    }
                    Some(PossiblyImage::Explicit(_)) => {
                        return Err(de::Error::custom(format_args!(
                            "field `added_layers` cannot be set with `layer` field"
                        )));
                    }
                    Some(PossiblyImage::Image) => {
                        added_layers = Some(map.next_value()?);
                    }
                },
                JobField::Devices => {
                    devices = Some(map.next_value()?);
                }
                JobField::Mounts => {
                    mounts = Some(map.next_value()?);
                }
                JobField::EnableLoopback => {
                    enable_loopback = Some(map.next_value()?);
                }
                JobField::EnableWritableFileSystem => {
                    enable_writable_file_system = Some(map.next_value()?);
                }
                JobField::WorkingDirectory => {
                    if working_directory.is_some() {
                        assert!(matches!(working_directory, Some(PossiblyImage::Image)));
                        return Err(de::Error::custom(format_args!(concat!(
                            "field `working_directory` cannot be set if `image` with a `use` of ",
                            "`working_directory` is also set"
                        ))));
                    }
                    working_directory = Some(PossiblyImage::Explicit(map.next_value()?));
                }
                JobField::User => {
                    user = Some(map.next_value()?);
                }
                JobField::Group => {
                    group = Some(map.next_value()?);
                }
                JobField::Image => {
                    let i = map.next_value::<DirectiveImage>()?;
                    image = Some(i.name);
                    for use_ in i.use_ {
                        match use_ {
                            ImageUse::WorkingDirectory => {
                                if working_directory.is_some() {
                                    assert!(matches!(
                                        working_directory,
                                        Some(PossiblyImage::Explicit(_))
                                    ));
                                    return Err(de::Error::custom(format_args!(
                                        "field `image` cannot use `working_directory` if field `working_directory` is also set"
                                    )));
                                }
                                working_directory = Some(PossiblyImage::Image);
                            }
                            ImageUse::Layers => {
                                if layers.is_some() {
                                    assert!(matches!(layers, Some(PossiblyImage::Explicit(_))));
                                    return Err(de::Error::custom(format_args!(
                                        "field `image` cannot use `layers` if field `layers` is also set"
                                    )));
                                }
                                layers = Some(PossiblyImage::Image);
                            }
                            ImageUse::Environment => {
                                if environment.is_some() {
                                    assert!(matches!(
                                        environment,
                                        Some(PossiblyImage::Explicit(_))
                                    ));
                                    return Err(de::Error::custom(format_args!(
                                        "field `image` cannot use `environment` if field `environment` is also set"
                                    )));
                                }
                                environment = Some(PossiblyImage::Image);
                            }
                        }
                    }
                }
            }
        }
        Ok(Job {
            program: program.ok_or_else(|| de::Error::missing_field("program"))?,
            arguments,
            environment,
            added_environment: added_environment.unwrap_or_default(),
            layers: layers.ok_or_else(|| de::Error::missing_field("layers"))?,
            added_layers: added_layers.unwrap_or_default(),
            devices,
            mounts,
            enable_loopback,
            enable_writable_file_system,
            working_directory,
            user,
            group,
            image,
        })
    }
}

impl<'de> de::Deserialize<'de> for Job {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JobVisitor)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::anyhow;
    use meticulous_base::{enum_set, nonempty, JobMountFsType};
    use meticulous_test::{digest, path_buf_vec};

    fn layer_mapper(layer: String) -> anyhow::Result<NonEmpty<Sha256Digest>> {
        Ok(nonempty![Sha256Digest::from(layer.parse::<u64>()?)])
    }

    fn env(var: &str) -> Result<Option<String>> {
        match var {
            "FOO" => Ok(Some("foo-env".to_string())),
            "err" => Err(anyhow!("error converting value to UTF-8")),
            _ => Ok(None),
        }
    }

    fn images(name: &str) -> Result<ContainerImage> {
        match name {
            "image1" => Ok(ContainerImage {
                layers: path_buf_vec!["42", "43"],
                working_directory: Some("/foo".into()),
                environment: Some(vec![
                    "FOO=image-foo".to_string(),
                    "BAZ=image-baz".to_string(),
                ]),
            }),
            "image-with-env-substitutions" => Ok(ContainerImage {
                environment: Some(vec!["PATH=$env{PATH}".to_string()]),
                ..Default::default()
            }),
            "empty" => Ok(Default::default()),
            _ => Err(anyhow!("no container named {name} found")),
        }
    }

    #[test]
    fn minimum_into_job_spec() {
        assert_eq!(
            Job::new("program".to_string(), nonempty!["1".to_string()])
                .into_job_spec(layer_mapper, env, images)
                .unwrap(),
            JobSpec::new("program", nonempty![digest!(1)]),
        );
    }

    #[test]
    fn most_into_job_spec() {
        assert_eq!(
            Job {
                arguments: Some(vec!["arg1".to_string(), "arg2".to_string()]),
                environment: Some(PossiblyImage::Explicit(BTreeMap::from([
                    ("FOO".to_string(), "foo".to_string()),
                    ("BAR".to_string(), "bar".to_string()),
                ]))),
                devices: Some(enum_set! {JobDeviceListDeserialize::Null}),
                mounts: Some(vec![JobMount {
                    fs_type: JobMountFsType::Tmp,
                    mount_point: "/tmp".into()
                }]),
                working_directory: Some(PossiblyImage::Explicit("/working-directory".into())),
                user: Some(UserId::from(101)),
                group: Some(GroupId::from(202)),
                ..Job::new("program".to_string(), nonempty!["1".to_string()])
            }
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("program", nonempty![digest!(1)])
                .arguments(["arg1", "arg2"])
                .environment(["BAR=bar", "FOO=foo"])
                .devices(enum_set! {JobDevice::Null})
                .mounts([JobMount {
                    fs_type: JobMountFsType::Tmp,
                    mount_point: "/tmp".into()
                }])
                .working_directory("/working-directory")
                .user(101)
                .group(202),
        );
    }

    #[test]
    fn enable_loopback_into_job_spec() {
        assert_eq!(
            Job {
                enable_loopback: Some(true),
                ..Job::new("program".to_string(), nonempty!["1".to_string()])
            }
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("program", nonempty![digest!(1)]).enable_loopback(true),
        );
    }

    #[test]
    fn enable_writable_file_system_into_job_spec() {
        assert_eq!(
            Job {
                enable_writable_file_system: Some(true),
                ..Job::new("program".to_string(), nonempty!["1".to_string()])
            }
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("program", nonempty![digest!(1)]).enable_writable_file_system(true),
        );
    }

    fn parse_job(str_: &str) -> serde_json::Result<Job> {
        serde_json::from_str(str_)
    }

    fn assert_error(err: serde_json::Error, expected: &str) {
        let message = format!("{err}");
        assert!(
            message.starts_with(expected),
            "message: {message:?}, expected: {expected:?}"
        );
    }

    fn assert_anyhow_error(err: anyhow::Error, expected: &str) {
        let message = format!("{err}");
        assert!(
            message.starts_with(expected),
            "message: {message:?}, expected: {expected:?}"
        );
    }

    #[test]
    fn basic() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ]
                }"#
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]),
        );
    }

    #[test]
    fn missing_program() {
        assert_error(
            parse_job(
                r#"{
                    "layers": [ "1" ]
                }"#,
            )
            .unwrap_err(),
            "missing field `program`",
        );
    }

    #[test]
    fn missing_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh"
                }"#,
            )
            .unwrap_err(),
            "missing field `layers`",
        );
    }

    #[test]
    fn empty_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": []
                }"#,
            )
            .unwrap_err(),
            "field `layers` cannot be empty",
        );
    }

    #[test]
    fn layers_from_image() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "image": {
                        "name": "image1",
                        "use": [ "layers" ]
                    }
                }"#
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(42), digest!(43)]),
        );
    }

    #[test]
    fn empty_layers_from_image() {
        assert_anyhow_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "image": {
                        "name": "empty",
                        "use": [ "layers" ]
                    }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap_err(),
            "image empty has no layers to use",
        );
    }

    #[test]
    fn layers_after_layers_from_image() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "image": {
                        "name": "image1",
                        "use": [ "layers" ]
                    },
                    "layers": [ "1" ]
                }"#,
            )
            .unwrap_err(),
            "field `layers` cannot be set if `image` with a `use` of `layers` is also set",
        );
    }

    #[test]
    fn layers_from_image_after_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": {
                        "name": "image1",
                        "use": [ "layers" ]
                    }
                }"#,
            )
            .unwrap_err(),
            "field `image` cannot use `layers` if field `layers` is also set",
        );
    }

    #[test]
    fn added_layers_after_layers_from_image() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "image": {
                        "name": "image1",
                        "use": [ "layers" ]
                    },
                    "added_layers": [ "1" ]
                }"#
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new(
                "/bin/sh".to_string(),
                nonempty![digest!(42), digest!(43), digest!(1)]
            ),
        );
    }

    #[test]
    fn added_layers_only() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "added_layers": [ "1" ]
                }"#,
            )
            .unwrap_err(),
            "field `added_layers` set before `image` with a `use` of `layers`",
        );
    }

    #[test]
    fn added_layers_before_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "added_layers": [ "3" ],
                    "layers": [ "1", "2" ]
                }"#,
            )
            .unwrap_err(),
            "field `added_layers` set before `image` with a `use` of `layers`",
        );
    }

    #[test]
    fn added_layers_after_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1", "2" ],
                    "added_layers": [ "3" ]
                }"#,
            )
            .unwrap_err(),
            "field `added_layers` cannot be set with `layer` field",
        );
    }

    #[test]
    fn added_layers_before_image_with_layers() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "added_layers": [ "3" ],
                    "image": { "name": "image1", "use": [ "layers" ] }
                }"#,
            )
            .unwrap_err(),
            "field `added_layers` set before `image` with a `use` of `layers`",
        );
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "arguments": [ "-e", "echo foo" ]
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .arguments(["-e", "echo foo"]),
        )
    }

    #[test]
    fn environment() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .environment(["BAR=bar", "FOO=foo"]),
        )
    }

    #[test]
    fn environment_with_env_substitution() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "environment": { "FOO": "pre-$env{FOO}-post", "BAR": "bar" }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .environment(["BAR=bar", "FOO=pre-foo-env-post"]),
        )
    }

    #[test]
    fn environment_with_prev_substitution() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "environment": { "FOO": "pre-$prev{FOO:-no-prev}-post", "BAR": "bar" }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .environment(["BAR=bar", "FOO=pre-no-prev-post"]),
        )
    }

    #[test]
    fn environment_from_image() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "image1", "use": [ "environment" ] }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .environment(["BAZ=image-baz", "FOO=image-foo"]),
        )
    }

    #[test]
    fn environment_from_image_ignores_env_substitutions() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "image-with-env-substitutions", "use": [ "environment" ] }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .environment(["PATH=$env{PATH}"]),
        )
    }

    #[test]
    fn environment_from_image_with_no_environment() {
        assert_anyhow_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "empty", "use": [ "environment" ] }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap_err(),
            "image empty has no environment to use",
        )
    }

    #[test]
    fn environment_from_image_after_environment() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "environment": { "FOO": "foo", "BAR": "bar" },
                    "image": { "name": "image1", "use": [ "environment" ] }
                }"#,
            )
            .unwrap_err(),
            "field `image` cannot use `environment` if field `environment` is also set",
        )
    }

    #[test]
    fn environment_after_environment_from_image() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "image1", "use": [ "environment" ] },
                    "environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap_err(),
            "field `environment` cannot be set if `image` with a `use` of `environment` is also set",
        )
    }

    #[test]
    fn added_environment_after_environment_from_image() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "image1", "use": [ "environment" ] },
                    "added_environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).environment([
                "BAR=bar",
                "BAZ=image-baz",
                "FOO=foo"
            ]),
        )
    }

    #[test]
    fn added_environment_after_environment_from_image_with_substitutions() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": { "name": "image1", "use": [ "environment" ] },
                    "added_environment": {
                        "FOO": "$env{FOO:-no-env-foo}:$prev{FOO:-no-prev-foo}",
                        "BAR": "$env{BAR:-no-env-bar}:$prev{BAR:-no-prev-bar}",
                        "BAZ": "$env{BAZ:-no-env-baz}:$prev{BAZ:-no-prev-baz}"
                    }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).environment([
                "BAR=no-env-bar:no-prev-bar",
                "BAZ=no-env-baz:image-baz",
                "FOO=foo-env:image-foo"
            ]),
        )
    }

    #[test]
    fn added_environment_without_environment_from_image() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "added_environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap_err(),
            "field `added_environment` set before `image` with a `use` of `environment`",
        )
    }

    #[test]
    fn added_environment_before_environment_from_image() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "added_environment": { "FOO": "foo", "BAR": "bar" },
                    "image": { "name": "image1", "use": [ "environment" ] }
                }"#,
            )
            .unwrap_err(),
            "field `added_environment` set before `image` with a `use` of `environment`",
        )
    }

    #[test]
    fn added_environment_before_environment() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "added_environment": { "FOO": "foo", "BAR": "bar" },
                    "environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap_err(),
            "field `added_environment` set before `image` with a `use` of `environment`",
        )
    }

    #[test]
    fn added_environment_after_environment() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "environment": { "FOO": "foo", "BAR": "bar" },
                    "added_environment": { "FOO": "foo", "BAR": "bar" }
                }"#,
            )
            .unwrap_err(),
            "field `added_environment` cannot be set with `environment` field",
        )
    }

    #[test]
    fn devices() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "devices": [ "null", "zero" ]
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .devices(enum_set! {JobDevice::Null | JobDevice::Zero}),
        )
    }

    #[test]
    fn mounts() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "mounts": [
                        { "fs_type": "tmp", "mount_point": "/tmp" }
                    ]
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).mounts([JobMount {
                fs_type: JobMountFsType::Tmp,
                mount_point: "/tmp".to_string()
            }])
        )
    }

    #[test]
    fn enable_loopback() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "enable_loopback": true
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).enable_loopback(true),
        )
    }

    #[test]
    fn enable_writable_file_system() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "enable_writable_file_system": true
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .enable_writable_file_system(true),
        )
    }

    #[test]
    fn working_directory() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "working_directory": "/foo/bar"
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)])
                .working_directory("/foo/bar"),
        )
    }

    #[test]
    fn working_directory_from_image() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": {
                        "name": "image1",
                        "use": [ "working_directory" ]
                    }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).working_directory("/foo"),
        )
    }

    #[test]
    fn working_directory_from_image_with_no_working_directory() {
        assert_anyhow_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": {
                        "name": "empty",
                        "use": [ "working_directory" ]
                    }
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap_err(),
            "image empty has no working_directory to use",
        )
    }

    #[test]
    fn working_directory_from_image_after_working_directory() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "working_directory": "/foo/bar",
                    "image": {
                        "name": "image1",
                        "use": [ "working_directory" ]
                    }
                }"#,
            )
            .unwrap_err(),
            "field `image` cannot use `working_directory` if field `working_directory` is also set",
        )
    }

    #[test]
    fn working_directory_after_working_directory_from_image() {
        assert_error(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "image": {
                        "name": "image1",
                        "use": [ "working_directory" ]
                    },
                    "working_directory": "/foo/bar"
                }"#,
            )
            .unwrap_err(),
            "field `working_directory` cannot be set if `image` with a `use` of `working_directory` is also set",
        )
    }

    #[test]
    fn user() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "user": 1234
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).user(1234),
        )
    }

    #[test]
    fn group() {
        assert_eq!(
            parse_job(
                r#"{
                    "program": "/bin/sh",
                    "layers": [ "1" ],
                    "group": 4321
                }"#,
            )
            .unwrap()
            .into_job_spec(layer_mapper, env, images)
            .unwrap(),
            JobSpec::new("/bin/sh".to_string(), nonempty![digest!(1)]).group(4321),
        )
    }
}