use clap::Parser;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "List supplementary Linux group IDs for this process")]
struct Arguments {
    #[arg(
        long,
        conflicts_with = "contains",
        help = "Print only the number of supplementary groups"
    )]
    count: bool,

    #[arg(long, value_name = "GID", value_parser = parse_gid, help = "Exit with status 0 for membership or 1 for absence")]
    contains: Option<u32>,
}

fn parse_gid(value: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("group ID must contain only decimal digits".to_owned());
    }
    value
        .parse()
        .map_err(|_| "group ID must be between 0 and 4294967295".to_owned())
}

#[cfg(any(target_os = "linux", test))]
fn retrieve_groups<F>(mut query: F) -> io::Result<Vec<u32>>
where
    F: FnMut(&mut [u32]) -> io::Result<usize>,
{
    for _ in 0..8 {
        let size = query(&mut [])?;
        if size > i32::MAX as usize {
            return Err(io::Error::other(
                "supplementary group count exceeds the system call limit",
            ));
        }
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(size)
            .map_err(|_| io::Error::other("could not allocate the supplementary group list"))?;
        groups.resize(size, 0);
        match query(&mut groups) {
            Ok(count) if count <= groups.len() => {
                groups.truncate(count);
                groups.sort_unstable();
                return Ok(groups);
            }
            Ok(_) => continue,
            Err(error) if error.raw_os_error() == Some(22) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "supplementary group list kept changing after 8 attempts",
    ))
}

#[cfg(target_os = "linux")]
fn supplementary_groups() -> io::Result<Vec<u32>> {
    use std::os::raw::{c_int, c_uint};

    unsafe extern "C" {
        fn getgroups(size: c_int, list: *mut c_uint) -> c_int;
    }

    retrieve_groups(|groups| {
        let size = c_int::try_from(groups.len()).map_err(|_| {
            io::Error::other("supplementary group count exceeds the system call limit")
        })?;
        let pointer = if groups.is_empty() {
            std::ptr::null_mut()
        } else {
            groups.as_mut_ptr()
        };
        let result = unsafe { getgroups(size, pointer) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn supplementary_groups() -> io::Result<Vec<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported platform: getgroups requires Linux",
    ))
}

fn run(arguments: Arguments) -> io::Result<ExitCode> {
    let groups = supplementary_groups()?;
    if let Some(gid) = arguments.contains {
        return Ok(if groups.binary_search(&gid).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if arguments.count {
        writeln!(output, "{}", groups.len())?;
    } else {
        for gid in groups {
            writeln!(output, "{gid}")?;
        }
    }
    output.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("getgroups: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_numeric_group_ids() {
        assert_eq!(parse_gid("0"), Ok(0));
        assert_eq!(parse_gid("4294967295"), Ok(u32::MAX));
        for invalid in ["", "not-a-gid", "-1", "+1", " 1", "1 ", "4294967296"] {
            assert!(parse_gid(invalid).is_err());
        }
    }

    #[test]
    fn rejects_conflicting_modes_and_invalid_ids() {
        assert!(Arguments::try_parse_from(["getgroups", "--count", "--contains", "1"]).is_err());
        assert!(Arguments::try_parse_from(["getgroups", "--contains", "not-a-gid"]).is_err());
        assert!(Arguments::try_parse_from(["getgroups", "--count"]).is_ok());
    }

    #[test]
    fn sorts_groups_and_handles_shrinkage() {
        let mut calls = 0;
        let groups = retrieve_groups(|buffer| {
            calls += 1;
            if calls == 1 {
                Ok(4)
            } else {
                buffer[..3].copy_from_slice(&[42, 2, 11]);
                Ok(3)
            }
        })
        .unwrap();
        assert_eq!(groups, [2, 11, 42]);
    }

    #[test]
    fn retries_when_the_group_list_grows() {
        let mut calls = 0;
        let groups = retrieve_groups(|buffer| {
            calls += 1;
            match calls {
                1 => Ok(1),
                2 => Err(io::Error::from_raw_os_error(22)),
                3 => Ok(2),
                4 => {
                    buffer.copy_from_slice(&[20, 10]);
                    Ok(2)
                }
                _ => panic!("unexpected query"),
            }
        })
        .unwrap();
        assert_eq!(groups, [10, 20]);
    }

    #[test]
    fn retries_growth_from_an_empty_list() {
        let mut calls = 0;
        let groups = retrieve_groups(|buffer| {
            calls += 1;
            match calls {
                1 => Ok(0),
                2 | 3 => Ok(1),
                4 => {
                    buffer[0] = 7;
                    Ok(1)
                }
                _ => panic!("unexpected query"),
            }
        })
        .unwrap();
        assert_eq!(groups, [7]);
    }

    #[test]
    fn limits_retries() {
        let mut calls = 0;
        let result = retrieve_groups(|buffer| {
            calls += 1;
            if buffer.is_empty() {
                Ok(1)
            } else {
                Err(io::Error::from_raw_os_error(22))
            }
        });
        assert!(result.is_err());
        assert_eq!(calls, 16);
    }

    #[test]
    fn propagates_kernel_failures() {
        let result = retrieve_groups(|_| Err(io::Error::from_raw_os_error(5)));
        assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
    }

    #[test]
    fn accepts_an_empty_group_list() {
        assert!(retrieve_groups(|_| Ok(0)).unwrap().is_empty());
    }
}
