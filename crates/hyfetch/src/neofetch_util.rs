use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::Write as _;
#[cfg(windows)]
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::{env, fmt};

use aho_corasick::AhoCorasick;
use anyhow::{anyhow, Context as _, Result};
use indexmap::IndexMap;
use itertools::Itertools as _;
#[cfg(windows)]
use normpath::PathExt as _;
#[cfg(windows)]
use same_file::is_same_file;
use serde::{Deserialize, Serialize};
use strum::AsRefStr;
use tempfile::NamedTempFile;
use tracing::debug;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::color_util::{
    color, printc, ForegroundBackground, NeofetchAsciiIndexedColor, PresetIndexedColor,
    ToAnsiString as _,
};
use crate::distros::Distro;
use crate::presets::ColorProfile;
use crate::types::{AnsiMode, Backend, TerminalTheme};
use crate::utils::{find_file, find_in_path, input, process_command_status};

pub const NEOFETCH_COLOR_PATTERNS: [&str; 6] =
    ["${c1}", "${c2}", "${c3}", "${c4}", "${c5}", "${c6}"];
pub static NEOFETCH_COLORS_AC: OnceLock<AhoCorasick> = OnceLock::new();

type ForeBackColorPair = (NeofetchAsciiIndexedColor, NeofetchAsciiIndexedColor);

#[derive(Clone, Eq, PartialEq, Debug, AsRefStr, Deserialize, Serialize)]
#[serde(tag = "mode")]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ColorAlignment {
    Horizontal {
        #[serde(skip)]
        fore_back: Option<ForeBackColorPair>,
    },
    Vertical {
        #[serde(skip)]
        fore_back: Option<ForeBackColorPair>,
    },
    Custom {
        #[serde(rename = "custom_colors")]
        #[serde(deserialize_with = "crate::utils::index_map_serde::deserialize")]
        colors: IndexMap<NeofetchAsciiIndexedColor, PresetIndexedColor>,
    },
}

impl ColorAlignment {
    /// Uses the color alignment to recolor an ascii art.
    #[tracing::instrument(level = "debug", skip(asc))]
    pub fn recolor_ascii<S>(
        &self,
        asc: S,
        color_profile: &ColorProfile,
        color_mode: AnsiMode,
        theme: TerminalTheme,
    ) -> Result<String>
    where
        S: AsRef<str>,
    {
        debug!("recolor ascii");

        let reset = color("&~&*", color_mode).expect("color reset should not be invalid");

        let asc = match self {
            &Self::Horizontal {
                fore_back: Some((fore, back)),
            } => {
                let asc = fill_starting(asc)
                    .context("failed to fill in starting neofetch color codes")?;

                // Replace foreground colors
                let asc = asc.replace(
                    &format!("${{c{fore}}}", fore = u8::from(fore)),
                    &color(
                        match theme {
                            TerminalTheme::Light => "&0",
                            TerminalTheme::Dark => "&f",
                        },
                        color_mode,
                    )
                    .expect("foreground color should not be invalid"),
                );

                // Add new colors
                let asc = {
                    let ColorProfile { colors } = {
                        let (_, length) = ascii_size(&asc);
                        color_profile
                            .with_length(length)
                            .context("failed to spread color profile to length")?
                    };
                    asc.split('\n')
                        .enumerate()
                        .map(|(i, line)| {
                            let line = line.replace(
                                &format!("${{c{back}}}", back = u8::from(back)),
                                &colors[i].to_ansi_string(color_mode, {
                                    // note: this is "background" in the ascii art, but
                                    // foreground text in terminal
                                    ForegroundBackground::Foreground
                                }),
                            );
                            format!("{line}{reset}")
                        })
                        .join("\n")
                };

                // Remove existing colors
                let asc = {
                    let ac = NEOFETCH_COLORS_AC
                        .get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());
                    const N: usize = NEOFETCH_COLOR_PATTERNS.len();
                    const REPLACEMENTS: [&str; N] = [""; N];
                    ac.replace_all(&asc, &REPLACEMENTS)
                };

                asc
            },
            &Self::Vertical {
                fore_back: Some((fore, back)),
            } => {
                let asc = fill_starting(asc)
                    .context("failed to fill in starting neofetch color codes")?;

                let color_profile = {
                    let (length, _) = ascii_size(&asc);
                    color_profile
                        .with_length(length)
                        .context("failed to spread color profile to length")?
                };

                // Apply colors
                let asc = {
                    let ac = NEOFETCH_COLORS_AC
                        .get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());
                    asc.split('\n')
                        .map(|line| {
                            let mut matches = ac.find_iter(line).peekable();
                            let mut dst = String::new();
                            let mut offset = 0;
                            loop {
                                let current = matches.next();
                                let next = matches.peek();
                                let (neofetch_color_idx, span, done) = match (current, next) {
                                    (Some(m), Some(m_next)) => {
                                        let neofetch_color_idx: NeofetchAsciiIndexedColor = line
                                            [m.start() + 3..m.end() - 1]
                                            .parse()
                                            .expect("neofetch color index should be valid");
                                        offset += m.len();
                                        let mut span = m.span();
                                        span.start = m.end();
                                        span.end = m_next.start();
                                        (neofetch_color_idx, span, false)
                                    },
                                    (Some(m), None) => {
                                        // Last color code
                                        let neofetch_color_idx: NeofetchAsciiIndexedColor = line
                                            [m.start() + 3..m.end() - 1]
                                            .parse()
                                            .expect("neofetch color index should be valid");
                                        offset += m.len();
                                        let mut span = m.span();
                                        span.start = m.end();
                                        span.end = line.len();
                                        (neofetch_color_idx, span, true)
                                    },
                                    (None, _) => {
                                        // No color code in the entire line
                                        unreachable!(
                                            "`fill_starting` ensured each line of ascii art \
                                             starts with neofetch color code"
                                        );
                                    },
                                };
                                let txt = &line[span];

                                if neofetch_color_idx == fore {
                                    let fore = color(
                                        match theme {
                                            TerminalTheme::Light => "&0",
                                            TerminalTheme::Dark => "&f",
                                        },
                                        color_mode,
                                    )
                                    .expect("foreground color should not be invalid");
                                    write!(dst, "{fore}{txt}{reset}").unwrap();
                                } else if neofetch_color_idx == back {
                                    dst.push_str(
                                        &ColorProfile::new(Vec::from(
                                            &color_profile.colors
                                                [span.start - offset..span.end - offset],
                                        ))
                                        .color_text(
                                            txt,
                                            color_mode,
                                            ForegroundBackground::Foreground,
                                            false,
                                        )
                                        .context("failed to color text using color profile")?,
                                    );
                                } else {
                                    dst.push_str(txt);
                                }

                                if done {
                                    break;
                                }
                            }
                            Ok(dst)
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join("\n")
                };

                asc
            },
            Self::Horizontal { fore_back: None } | Self::Vertical { fore_back: None } => {
                // Remove existing colors
                let asc = {
                    let ac = NEOFETCH_COLORS_AC
                        .get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());
                    const N: usize = NEOFETCH_COLOR_PATTERNS.len();
                    const REPLACEMENTS: [&str; N] = [""; N];
                    ac.replace_all(asc.as_ref(), &REPLACEMENTS)
                };

                let lines: Vec<_> = asc.split('\n').collect();

                // Add new colors
                match self {
                    Self::Horizontal { .. } => {
                        let ColorProfile { colors } = {
                            let (_, length) = ascii_size(&asc);
                            color_profile
                                .with_length(length)
                                .context("failed to spread color profile to length")?
                        };
                        lines
                            .into_iter()
                            .enumerate()
                            .map(|(i, line)| {
                                let fore = colors[i]
                                    .to_ansi_string(color_mode, ForegroundBackground::Foreground);
                                format!("{fore}{line}{reset}")
                            })
                            .join("\n")
                    },
                    Self::Vertical { .. } => lines
                        .into_iter()
                        .map(|line| {
                            let line = color_profile
                                .color_text(
                                    line,
                                    color_mode,
                                    ForegroundBackground::Foreground,
                                    false,
                                )
                                .context("failed to color text using color profile")?;
                            Ok(line)
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join("\n"),
                    _ => {
                        unreachable!();
                    },
                }
            },
            Self::Custom {
                colors: custom_colors,
            } => {
                let asc = fill_starting(asc)
                    .context("failed to fill in starting neofetch color codes")?;

                let ColorProfile { colors } = color_profile.unique_colors();

                // Apply colors
                let asc = {
                    let ac = NEOFETCH_COLORS_AC
                        .get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());
                    const N: usize = NEOFETCH_COLOR_PATTERNS.len();
                    let mut replacements = vec![Cow::from(""); N];
                    for (&ai, &pi) in custom_colors {
                        let ai = u8::from(ai);
                        let pi = u8::from(pi);
                        replacements[usize::from(ai - 1)] = colors[usize::from(pi)]
                            .to_ansi_string(color_mode, ForegroundBackground::Foreground)
                            .into();
                    }
                    ac.replace_all(&asc, &replacements)
                };

                // Reset colors at end of each line to prevent color bleeding
                let asc = asc
                    .split('\n')
                    .map(|line| format!("{line}{reset}"))
                    .join("\n");

                asc
            },
        };

        Ok(asc)
    }

    /// Gets recommended foreground-background configuration for distro, or
    /// `None` if the distro ascii is not suitable for fore-back configuration.
    pub fn fore_back(distro: Distro) -> Option<ForeBackColorPair> {
        match distro {
            Distro::Anarchy
            | Distro::ArchStrike
            | Distro::Astra_Linux
            | Distro::Chapeau
            | Distro::Fedora
            | Distro::GalliumOS
            | Distro::KrassOS
            | Distro::Kubuntu
            | Distro::Lubuntu
            | Distro::openEuler
            | Distro::Peppermint
            | Distro::Pop__OS
            | Distro::Ubuntu_Cinnamon
            | Distro::Ubuntu_Kylin
            | Distro::Ubuntu_MATE
            | Distro::Ubuntu_old
            | Distro::Ubuntu_Studio
            | Distro::Ubuntu_Sway
            | Distro::Ultramarine_Linux
            | Distro::Univention
            | Distro::Vanilla
            | Distro::Xubuntu => Some((2, 1)),

            Distro::Antergos => Some((1, 2)),

            _ => None,
        }
        .map(|(fore, back): (u8, u8)| {
            (
                fore.try_into()
                    .expect("`fore` should be a valid neofetch color index"),
                back.try_into()
                    .expect("`back` should be a valid neofetch color index"),
            )
        })
    }
}

/// Asks the user to provide an input among a list of options.
pub fn literal_input<'a, S1, S2>(
    prompt: S1,
    options: &'a [S2],
    default: &str,
    show_options: bool,
    color_mode: AnsiMode,
) -> Result<&'a str>
where
    S1: AsRef<str>,
    S2: AsRef<str>,
{
    let prompt = prompt.as_ref();

    if show_options {
        let options_text = options
            .iter()
            .map(|o| {
                let o = o.as_ref();

                if o == default {
                    format!("&l&n{o}&L&N")
                } else {
                    o.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("|");
        printc(format!("{prompt} ({options_text})"), color_mode)
            .context("failed to print input prompt")?;
    } else {
        printc(format!("{prompt} (default: {default})"), color_mode)
            .context("failed to print input prompt")?;
    }

    loop {
        let selection = input(Some("> ")).context("failed to read input")?;
        let selection = if selection.is_empty() {
            default.to_owned()
        } else {
            selection.to_lowercase()
        };

        if let Some(selected) = find_selection(&selection, options) {
            println!();

            return Ok(selected);
        } else {
            let options_text = options.iter().map(AsRef::as_ref).join("|");
            println!("Invalid selection! {selection} is not one of {options_text}");
        }
    }

    fn find_selection<'a, S>(sel: &str, options: &'a [S]) -> Option<&'a str>
    where
        S: AsRef<str>,
    {
        if sel.is_empty() {
            return None;
        }

        // Find exact match
        if let Some(selected) = options.iter().find(|&o| o.as_ref().to_lowercase() == sel) {
            return Some(selected.as_ref());
        }

        // Find starting abbreviation
        if let Some(selected) = options
            .iter()
            .find(|&o| o.as_ref().to_lowercase().starts_with(sel))
        {
            return Some(selected.as_ref());
        }

        None
    }
}

/// Gets the absolute path of the neofetch command.
pub fn neofetch_path() -> Result<Option<PathBuf>> {
    if let Some(workspace_dir) = env::var_os("CARGO_WORKSPACE_DIR") {
        debug!(
            ?workspace_dir,
            "CARGO_WORKSPACE_DIR env var is set; using neofetch from project directory"
        );
        let workspace_path = Path::new(&workspace_dir);
        let workspace_path = match workspace_path.try_exists() {
            Ok(true) => workspace_path,
            Ok(false) => {
                return Err(anyhow!(
                    "{workspace_path:?} does not exist or is not readable"
                ));
            },
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to check existence of {workspace_path:?}"));
            },
        };
        let neofetch_path = workspace_path.join("neofetch");
        return find_file(&neofetch_path)
            .with_context(|| format!("failed to check existence of file {neofetch_path:?}"));
    }

    let neowofetch_path = find_in_path("neowofetch")
        .context("failed to check existence of `neowofetch` in `PATH`")?;

    // Fall back to `neowofetch` in directory of current executable
    let neowofetch_path = if neowofetch_path.is_some() {
        neowofetch_path
    } else {
        let current_exe_path: PathBuf = env::current_exe()
            .and_then(|p| {
                #[cfg(not(windows))]
                {
                    p.canonicalize()
                }
                #[cfg(windows)]
                {
                    p.normalize().map(|p| p.into())
                }
            })
            .context("failed to get path of current running executable")?;
        let neowofetch_path = current_exe_path
            .parent()
            .expect("parent should not be `None`")
            .join("neowofetch");
        find_file(&neowofetch_path)
            .with_context(|| format!("failed to check existence of file {neowofetch_path:?}"))?
    };

    Ok(neowofetch_path)
}

/// Ensures git bash installation for Windows.
///
/// Returns the path to git bash.
#[cfg(windows)]
pub fn ensure_git_bash() -> Result<PathBuf> {
    // Find `bash.exe` in `PATH`, but exclude the known bad paths
    let git_bash_path = {
        let bash_path = find_in_path("bash.exe")
            .context("failed to check existence of `bash.exe` in `PATH`")?;
        match bash_path {
            Some(bash_path) if bash_path.ends_with(r"Git\usr\bin\bash.exe") => {
                // See https://stackoverflow.com/a/58418686/1529493
                None
            },
            Some(bash_path) => {
                // See https://github.com/hykilpikonna/hyfetch/issues/233
                let windir = env::var_os("windir")
                    .context("`windir` environment variable is not set or invalid")?;
                match is_same_file(&bash_path, Path::new(&windir).join(r"System32\bash.exe")) {
                    Ok(true) => None,
                    Ok(false) => Some(bash_path),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => Some(bash_path),
                    Err(err) => {
                        return Err(err).context("failed to check if paths refer to the same file");
                    },
                }
            },
            _ => bash_path,
        }
    };

    // Detect any Git for Windows installation in `PATH`
    let git_bash_path = if git_bash_path.is_some() {
        git_bash_path
    } else {
        let git_path =
            find_in_path("git.exe").context("failed to check existence of `git.exe` in `PATH`")?;
        match git_path {
            Some(git_path) if git_path.ends_with(r"Git\cmd\git.exe") => {
                let bash_path = git_path
                    .parent()
                    .expect("parent should not be `None`")
                    .parent()
                    .expect("parent should not be `None`")
                    .join(r"bin\bash.exe");
                if bash_path.is_file() {
                    Some(bash_path)
                } else {
                    None
                }
            },
            _ => None,
        }
    };

    // Fall back to default Git for Windows installation paths
    let git_bash_path = git_bash_path
        .or_else(|| {
            let program_files_dir = env::var_os("ProgramFiles")?;
            let bash_path = Path::new(&program_files_dir).join(r"Git\bin\bash.exe");
            if bash_path.is_file() {
                Some(bash_path)
            } else {
                None
            }
        })
        .or_else(|| {
            let program_files_x86_dir = env::var_os("ProgramFiles(x86)")?;
            let bash_path = Path::new(&program_files_x86_dir).join(r"Git\bin\bash.exe");
            if bash_path.is_file() {
                Some(bash_path)
            } else {
                None
            }
        });

    // Bundled git bash
    let git_bash_path = if git_bash_path.is_some() {
        git_bash_path
    } else {
        let current_exe_path: PathBuf = env::current_exe()
            .and_then(|p| p.normalize().map(|p| p.into()))
            .context("failed to get path of current running executable")?;
        let bash_path = current_exe_path
            .parent()
            .expect("parent should not be `None`")
            .join(r"git\bin\bash.exe");
        if bash_path.is_file() {
            Some(bash_path)
        } else {
            None
        }
    };

    let git_bash_path = git_bash_path.context("failed to find git bash executable")?;

    Ok(git_bash_path)
}

pub fn fastfetch_path() -> Result<Option<PathBuf>> {
    let fastfetch_path =
        find_in_path("fastfetch").context("failed to check existence of `fastfetch` in `PATH`")?;
    #[cfg(windows)]
    let fastfetch_path = if fastfetch_path.is_some() {
        fastfetch_path
    } else {
        find_in_path("fastfetch.exe")
            .context("failed to check existence of `fastfetch.exe` in `PATH`")?
    };

    // Fall back to `fastfetch` in directory of current executable
    let current_exe_path: PathBuf = env::current_exe()
        .and_then(|p| {
            #[cfg(not(windows))]
            {
                p.canonicalize()
            }
            #[cfg(windows)]
            {
                p.normalize().map(|p| p.into())
            }
        })
        .context("failed to get path of current running executable")?;
    let current_exe_dir_path = current_exe_path
        .parent()
        .expect("parent should not be `None`");
    let fastfetch_path = if fastfetch_path.is_some() {
        fastfetch_path
    } else {
        let fastfetch_path = current_exe_dir_path.join("fastfetch");
        find_file(&fastfetch_path)
            .with_context(|| format!("failed to check existence of file {fastfetch_path:?}"))?
    };

    // Bundled fastfetch
    let fastfetch_path = if fastfetch_path.is_some() {
        fastfetch_path
    } else {
        let fastfetch_path = current_exe_dir_path.join("fastfetch/usr/bin/fastfetch");
        find_file(&fastfetch_path)
            .with_context(|| format!("failed to check existence of file {fastfetch_path:?}"))?
    };
    let fastfetch_path = if fastfetch_path.is_some() {
        fastfetch_path
    } else {
        let fastfetch_path = current_exe_dir_path.join("fastfetch/fastfetch");
        find_file(&fastfetch_path)
            .with_context(|| format!("failed to check existence of file {fastfetch_path:?}"))?
    };
    #[cfg(windows)]
    let fastfetch_path = if fastfetch_path.is_some() {
        fastfetch_path
    } else {
        let fastfetch_path = current_exe_dir_path.join(r"fastfetch\fastfetch.exe");
        find_file(&fastfetch_path)
            .with_context(|| format!("failed to check existence of file {fastfetch_path:?}"))?
    };

    Ok(fastfetch_path)
}

/// Gets the distro ascii of the current distro. Or if distro is specified, get
/// the specific distro's ascii art instead.
#[tracing::instrument(level = "debug")]
pub fn get_distro_ascii<S>(
    distro: Option<S>,
    backend: Backend,
) -> Result<(String, Option<ForeBackColorPair>)>
where
    S: AsRef<str> + fmt::Debug,
{
    let distro: Cow<_> = if let Some(distro) = distro.as_ref() {
        distro.as_ref().into()
    } else {
        get_distro_name(backend)
            .context("failed to get distro name")?
            .into()
    };
    debug!(%distro, "distro name");

    // Try new codegen-based detection method
    if let Some(distro) = Distro::detect(&distro) {
        return Ok((
            normalize_ascii(distro.ascii_art()),
            ColorAlignment::fore_back(distro),
        ));
    }

    debug!(%distro, "could not find a match for distro; falling back to neofetch");

    // Old detection method that calls neofetch
    let asc = run_neofetch_command_piped(&["print_ascii", "--ascii_distro", distro.as_ref()])
        .context("failed to get ascii art from neofetch")?;

    // Unescape backslashes here because backslashes are escaped in neofetch for
    // printf
    let asc = asc.replace(r"\\", r"\");

    Ok((normalize_ascii(asc), None))
}

#[tracing::instrument(level = "debug", skip(asc))]
pub fn run(asc: String, backend: Backend, args: Option<&Vec<String>>) -> Result<()> {
    match backend {
        Backend::Neofetch => {
            run_neofetch(asc, args).context("failed to run neofetch")?;
        },
        Backend::Fastfetch => {
            run_fastfetch(asc, args, false).context("failed to run fastfetch")?;
        },
        Backend::FastfetchOld => {
            run_fastfetch(asc, args, true).context("failed to run fastfetch")?;
        },
        Backend::Qwqfetch => {
            todo!();
        },
    }

    Ok(())
}

/// Gets distro ascii width and height, ignoring color code.
pub fn ascii_size<S>(asc: S) -> (u8, u8)
where
    S: AsRef<str>,
{
    let asc = asc.as_ref();

    let asc = {
        let ac =
            NEOFETCH_COLORS_AC.get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());
        const N: usize = NEOFETCH_COLOR_PATTERNS.len();
        const REPLACEMENTS: [&str; N] = [""; N];
        ac.replace_all(asc, &REPLACEMENTS)
    };

    let width = asc
        .split('\n')
        .map(|line| line.graphemes(true).count())
        .max()
        .expect("line iterator should not be empty");
    let width = u8::try_from(width).expect("`width` should fit in `u8`");
    let height = asc.split('\n').count();
    let height = u8::try_from(height).expect("`height` should fit in `u8`");

    (width, height)
}

/// Makes sure every line are the same width.
fn normalize_ascii<S>(asc: S) -> String
where
    S: AsRef<str>,
{
    let asc = asc.as_ref();

    let (w, _) = ascii_size(asc);

    asc.split('\n')
        .map(|line| {
            let (line_w, _) = ascii_size(line);
            let pad = " ".repeat(usize::from(w - line_w));
            format!("{line}{pad}")
        })
        .join("\n")
}

/// Fills the missing starting placeholders.
///
/// e.g. `"${c1}...\n..."` -> `"${c1}...\n${c1}..."`
fn fill_starting<S>(asc: S) -> Result<String>
where
    S: AsRef<str>,
{
    let asc = asc.as_ref();

    let ac = NEOFETCH_COLORS_AC.get_or_init(|| AhoCorasick::new(NEOFETCH_COLOR_PATTERNS).unwrap());

    let mut last = None;
    Ok(asc
        .split('\n')
        .map(|line| {
            let mut new = String::new();
            let mut matches = ac.find_iter(line).peekable();

            match matches.peek() {
                Some(m)
                    if m.start() == 0 || line[0..m.start()].trim_end_matches(' ').is_empty() =>
                {
                    // line starts with neofetch color code, do nothing
                },
                _ => {
                    new.push_str(
                        last.context("failed to find neofetch color code from a previous line")?,
                    );
                },
            }
            new.push_str(line);

            // Get the last placeholder for the next line
            if let Some(m) = matches.last() {
                last = Some(&line[m.span()])
            }

            Ok(new)
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n"))
}

/// Runs neofetch command, returning the piped stdout output.
fn run_neofetch_command_piped<S>(args: &[S]) -> Result<String>
where
    S: AsRef<OsStr> + fmt::Debug,
{
    let mut command = make_neofetch_command(args)?;

    let output = command
        .output()
        .context("failed to execute neofetch as child process")?;
    debug!(?output, "neofetch output");
    process_command_status(&output.status).context("neofetch command exited with error")?;

    let out = String::from_utf8(output.stdout)
        .context("failed to process neofetch output as it contains invalid UTF-8")?
        .trim()
        .to_owned();
    Ok(out)
}

fn make_neofetch_command<S>(args: &[S]) -> Result<Command>
where
    S: AsRef<OsStr>,
{
    let neofetch_path = neofetch_path().context("failed to get neofetch path")?;
    let neofetch_path = neofetch_path.context("neofetch command not found")?;

    debug!(?neofetch_path, "neofetch path");

    #[cfg(not(windows))]
    {
        let mut command = Command::new("bash");
        command.arg(neofetch_path);
        command.args(args);
        Ok(command)
    }
    #[cfg(windows)]
    {
        let git_bash_path = ensure_git_bash().context("failed to get git bash path")?;
        let mut command = Command::new(git_bash_path);
        command.arg(neofetch_path);
        command.args(args);
        Ok(command)
    }
}

/// Runs fastfetch command, returning the piped stdout output.
fn run_fastfetch_command_piped<S>(args: &[S]) -> Result<String>
where
    S: AsRef<OsStr> + fmt::Debug,
{
    let mut command = make_fastfetch_command(args)?;

    let output = command
        .output()
        .context("failed to execute fastfetch as child process")?;
    debug!(?output, "fastfetch output");
    process_command_status(&output.status).context("fastfetch command exited with error")?;

    let out = String::from_utf8(output.stdout)
        .context("failed to process fastfetch output as it contains invalid UTF-8")?
        .trim()
        .to_owned();
    Ok(out)
}

fn make_fastfetch_command<S>(args: &[S]) -> Result<Command>
where
    S: AsRef<OsStr>,
{
    // Find fastfetch binary
    let fastfetch_path = fastfetch_path().context("failed to get fastfetch path")?;
    let fastfetch_path = fastfetch_path.context("fastfetch command not found")?;

    debug!(?fastfetch_path, "fastfetch path");

    let mut command = Command::new(fastfetch_path);
    command.args(args);
    Ok(command)
}

#[tracing::instrument(level = "debug")]
fn get_distro_name(backend: Backend) -> Result<String> {
    match backend {
        Backend::Neofetch => run_neofetch_command_piped(&["ascii_distro_name"])
            .context("failed to get distro name from neofetch"),
        Backend::Fastfetch | Backend::FastfetchOld => run_fastfetch_command_piped(&[
            "--logo",
            "none",
            "-s",
            "OS",
            "--disable-linewrap",
            "--os-key",
            " ",
        ])
        .context("failed to get distro name from fastfetch"),
        Backend::Qwqfetch => {
            todo!()
        },
    }
}

/// Runs neofetch with colors.
#[tracing::instrument(level = "debug", skip(asc))]
fn run_neofetch(asc: String, args: Option<&Vec<String>>) -> Result<()> {
    // Escape backslashes here because backslashes are escaped in neofetch for
    // printf
    let asc = asc.replace('\\', r"\\");

    // Write temp file
    let mut temp_file =
        NamedTempFile::with_prefix("ascii.txt").context("failed to create temp file for ascii")?;
    temp_file
        .write_all(asc.as_bytes())
        .context("failed to write ascii to temp file")?;

    // Call neofetch with the temp file
    let temp_file_path = temp_file.into_temp_path();
    let args = {
        let mut v: Vec<Cow<OsStr>> = vec![
            OsStr::new("--ascii").into(),
            OsStr::new("--source").into(),
            OsStr::new(&temp_file_path).into(),
            OsStr::new("--ascii-colors").into(),
        ];
        if let Some(args) = args {
            v.extend(args.iter().map(|arg| OsStr::new(arg).into()));
        }
        v
    };
    let mut command = make_neofetch_command(&args[..])?;

    debug!(?command, "neofetch command");

    let status = command
        .status()
        .context("failed to execute neofetch command as child process")?;
    process_command_status(&status).context("neofetch command exited with error")?;

    Ok(())
}

/// Runs fastfetch with colors.
#[tracing::instrument(level = "debug", skip(asc))]
fn run_fastfetch(asc: String, args: Option<&Vec<String>>, legacy: bool) -> Result<()> {
    // Write temp file
    let mut temp_file =
        NamedTempFile::with_prefix("ascii.txt").context("failed to create temp file for ascii")?;
    temp_file
        .write_all(asc.as_bytes())
        .context("failed to write ascii to temp file")?;

    // Call fastfetch with the temp file
    let temp_file_path = temp_file.into_temp_path();
    let args = {
        let mut v: Vec<Cow<OsStr>> = vec![
            OsStr::new(if legacy { "--raw" } else { "--file-raw" }).into(),
            OsStr::new(&temp_file_path).into(),
        ];
        if let Some(args) = args {
            v.extend(args.iter().map(|arg| OsStr::new(arg).into()));
        }
        v
    };
    let mut command = make_fastfetch_command(&args[..])?;

    debug!(?command, "fastfetch command");

    let status = command
        .status()
        .context("failed to execute fastfetch command as child process")?;
    if status.code() == Some(144) {
        eprintln!(
            "exit code 144 detected; please upgrade fastfetch to >=1.8.0 or use the \
             'fastfetch-old' backend"
        );
    }
    process_command_status(&status).context("fastfetch command exited with error")?;

    Ok(())
}
