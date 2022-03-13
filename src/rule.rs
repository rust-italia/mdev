use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    path::{Path, MAIN_SEPARATOR},
};

use kobject_uevent::ActionType;

use mdev_parser::{Conf, Filter, OnCreation};

use tokio::fs;

use tracing::{debug, info};

pub async fn apply<'a>(
    rule: &Conf,
    env: &HashMap<String, String>,
    device_number: Option<(u32, u32)>,
    action: ActionType,
    devpath: &Path,
    devname: &'a str,
) -> anyhow::Result<Option<Cow<'a, str>>> {
    if !rule.envmatches.iter().all(|env_match| {
        env.get(&env_match.envvar)
            .map(|var| env_match.regex.is_match(var))
            .unwrap_or(false)
    }) {
        return Ok(None);
    }

    // to avoid unneeded allocations
    let mut on_creation: Option<Cow<OnCreation>> = rule.on_creation.as_ref().map(Cow::Borrowed);

    match rule.filter {
        Filter::MajMin(ref device_number_match) => {
            if let Some((maj, min)) = device_number {
                if maj != device_number_match.maj {
                    return Ok(None);
                }
                let min2 = device_number_match.min2.unwrap_or(device_number_match.min);
                if min < device_number_match.min || min > min2 {
                    return Ok(None);
                }
            }
        }
        Filter::DeviceRegex(ref device_regex) => {
            let var = if let Some(ref envvar) = device_regex.envvar {
                if let Some(var) = env.get(envvar) {
                    var
                } else {
                    return Ok(None);
                }
            } else {
                devname
            };
            if let Some(old_on_creation) = on_creation {
                // this creates a sorted collection of usize:(String:&str)
                // because is lighter and quicker having matches already indexed
                // than converting to usize every substring that starts by % and contains numbers
                // the counterpart is that we allocate a string for every possible index
                let matches: BTreeMap<usize, (String, &str)> = device_regex
                    .regex
                    .find_iter(var)
                    .enumerate()
                    .map(|(index, m)| (index, (format!("%{}", index), m.as_str())))
                    .collect();
                if matches.is_empty() {
                    return Ok(None);
                }

                let mut new_on_creation = old_on_creation.into_owned();
                match &mut new_on_creation {
                    OnCreation::Move(pb) => {
                        replace_in_path(pb, &matches);
                    }
                    OnCreation::SymLink(pb) => {
                        replace_in_path(pb, &matches);
                    }
                    _ => {}
                }
                on_creation = Some(Cow::Owned(new_on_creation));
            } else if !device_regex.regex.is_match(var) {
                return Ok(None);
            }
        }
    }

    info!("rule matched {:?} action {:?}", rule, action);

    // WARNING: WIP code
    if let Some(creation) = on_creation.as_deref() {
        match creation {
            OnCreation::Move(to) | OnCreation::SymLink(to) => {
                debug!(
                    "{} {} to {}",
                    if let OnCreation::Move(_) = creation {
                        "Rename"
                    } else {
                        "Link"
                    },
                    devname,
                    to
                );
                let (dir, target) = if is_dir(to) {
                    (to.clone(), format!("{}{}", to, devname))
                } else {
                    let nsep = to.chars().filter(|c| *c == MAIN_SEPARATOR).count();
                    let mut n = 0;
                    let parent = to
                        .chars()
                        .take_while(|c| {
                            if *c == MAIN_SEPARATOR {
                                n += 1;
                            }
                            n < nsep
                        })
                        .collect();
                    (parent, to.clone())
                };
                fs::create_dir_all(devpath.join(dir)).await?;
                if let OnCreation::Move(_) = creation {
                    // fs::rename(devpath.join(devname), devpath.join(target)).await?;
                    return Ok(Some(Cow::Owned(target)));
                } else {
                    fs::symlink(devpath.join(devname), devpath.join(target)).await?;
                }
            }
            OnCreation::Prevent => {
                debug!("Do not create node");
                return Ok(None);
            }
        }
    }

    Ok(Some(Cow::Borrowed(devname)))
}

fn is_dir(path: &str) -> bool {
    // is this check enough?
    path.ends_with(MAIN_SEPARATOR)
}

fn replace_in_path(pb: &mut String, matches: &BTreeMap<usize, (String, &str)>) {
    // reverse iteration to go from highest number to lowest, therefore from longest to shortest
    // this way we replace %10 before %1
    for (_, (key, value)) in matches.iter().rev() {
        while let Some(pos) = pb.find(key) {
            pb.replace_range(pos..(pos + key.len()), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::HashMap, path::Path};

    use kobject_uevent::ActionType;

    use mdev_parser::{Conf, Filter, MajMin};

    #[tokio::test]
    async fn basic() {
        let conf = Conf {
            stop: false,
            envmatches: vec![],
            filter: Filter::MajMin(MajMin {
                maj: 0,
                min: 1,
                min2: None,
            }),
            user: String::from("root"),
            group: String::from("root"),
            mode: 0700,
            on_creation: None,
            command: None,
        };
        let env = HashMap::new();
        let devpath = Path::new("/dev");
        assert_eq!(
            super::apply(&conf, &env, None, ActionType::Add, &devpath, "foo")
                .await
                .unwrap(),
            Some(Cow::Borrowed("foo"))
        );
    }
}
