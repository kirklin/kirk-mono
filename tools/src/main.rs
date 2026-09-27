mod tables;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use anyhow::{Context, Result, bail};

use crate::tables::Instance;

const FAMILY: &str = "Kirk Mono";
const NO_LIGATURES_FAMILY: &str = "Kirk Mono NL";

/// The sources ask ttfautohint for `--increase-x-height=0`; `--no-info` keeps the version string clean.
const TTFAUTOHINT_ARGS: [&str; 2] = ["--increase-x-height=0", "--no-info"];

/// Programming ligatures live in `calt`, and `aalt` lists them again for glyph palettes.
const LIGATURE_FEATURES: &str = "calt,aalt";

const TOOLS: [&str; 4] = ["fontc", "hb-subset", "ttfautohint", "woff2_compress"];

struct Source {
    file: &'static str,
    italic: bool,
}

const SOURCES: [Source; 2] = [
    Source { file: "KirkMono.glyphs", italic: false },
    Source { file: "KirkMono-Italic.glyphs", italic: true },
];

struct Dirs {
    root: PathBuf,
    build: PathBuf,
    variable: PathBuf,
    ttf: PathBuf,
    webfonts: PathBuf,
}

fn main() -> Result<()> {
    for tool in TOOLS {
        require_tool(tool)?;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().context("tools/ has no parent")?.to_path_buf();
    let dirs = Dirs {
        build: root.join("build"),
        variable: root.join("fonts/variable"),
        ttf: root.join("fonts/ttf"),
        webfonts: root.join("fonts/webfonts"),
        root,
    };
    for dir in [dirs.build.clone(), dirs.root.join("fonts")] {
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
    }
    for dir in [&dirs.build, &dirs.build.join("woff2"), &dirs.variable, &dirs.ttf, &dirs.webfonts] {
        fs::create_dir_all(dir)?;
    }

    for source in &SOURCES {
        build_source(&dirs, source)?;
    }
    Ok(())
}

fn build_source(dirs: &Dirs, source: &Source) -> Result<()> {
    let stem = format!("{}{}[wght]", tables::postscript_family(FAMILY), if source.italic { "-Italic" } else { "" });
    let compiled = dirs.build.join(format!("{stem}.fontc.ttf"));
    run(Command::new("fontc")
        .arg(dirs.root.join("sources").join(source.file))
        .arg("--flatten-components=true")
        .arg("--build-dir")
        .arg(dirs.build.join(format!("fontc-{stem}")))
        .arg("--output-file")
        .arg(&compiled))?;

    let (variable, instances) = tables::finalize_variable(&fs::read(&compiled)?, FAMILY, source.italic)?;
    let unhinted = dirs.build.join(format!("{stem}.ttf"));
    fs::write(&unhinted, &variable)?;
    write(dirs, &dirs.variable.join(format!("{stem}.ttf")), &tables::add_unhinted_rendering_tables(&variable)?)?;

    thread::scope(|scope| {
        let builds: Vec<_> = instances
            .iter()
            .map(|instance| scope.spawn(|| build_static(dirs, &unhinted, instance, source.italic)))
            .collect();
        builds.into_iter().try_for_each(|build| build.join().expect("static build thread panicked"))
    })
}

fn build_static(dirs: &Dirs, variable: &Path, instance: &Instance, italic: bool) -> Result<()> {
    let style = tables::style_name(instance.weight, italic).replace(' ', "");
    let file = format!("{}-{style}", tables::postscript_family(FAMILY));

    let instanced = dirs.build.join(format!("{file}.instance.ttf"));
    run(Command::new("hb-subset")
        .arg("--keep-everything")
        .arg(format!("--variations=wght={:.4}", instance.location))
        .arg("--output-file")
        .arg(&instanced)
        .arg(variable))?;
    let named = dirs.build.join(format!("{file}.named.ttf"));
    fs::write(&named, tables::finalize_static(&fs::read(&instanced)?, FAMILY, instance, italic)?)?;
    let autohinted = dirs.build.join(format!("{file}.autohinted.ttf"));
    run(Command::new("ttfautohint").args(TTFAUTOHINT_ARGS).arg(&named).arg(&autohinted))?;
    let hinted = dirs.ttf.join(format!("{file}.ttf"));
    write(dirs, &hinted, &tables::stamp(&fs::read(&autohinted)?)?)?;

    let no_ligatures = format!("{}-{style}", tables::postscript_family(NO_LIGATURES_FAMILY));
    let subset = dirs.build.join(format!("{no_ligatures}.subset.ttf"));
    run(Command::new("hb-subset")
        .args(["--keep-everything", "--gids=", "--glyphs=", "--unicodes=*"])
        .arg(format!("--layout-features-={LIGATURE_FEATURES}"))
        .arg("--output-file")
        .arg(&subset)
        .arg(&hinted))?;
    let renamed = dirs.ttf.join(format!("{no_ligatures}.ttf"));
    fs::write(&renamed, tables::rename_static(&fs::read(&subset)?, NO_LIGATURES_FAMILY, instance, italic)?)?;
    report(dirs, &renamed);
    Ok(())
}

fn write(dirs: &Dirs, path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data)?;
    report(dirs, path);
    compress(dirs, path)
}

/// `woff2_compress` writes next to its input, so it works on a copy inside build/.
fn compress(dirs: &Dirs, ttf: &Path) -> Result<()> {
    let staged = dirs.build.join("woff2").join(ttf.file_name().context("font path has no file name")?);
    fs::copy(ttf, &staged)?;
    run(Command::new("woff2_compress").arg(&staged))?;
    let woff2 = staged.with_extension("woff2");
    let target = dirs.webfonts.join(woff2.file_name().unwrap());
    fs::rename(&woff2, &target)?;
    report(dirs, &target);
    Ok(())
}

fn report(dirs: &Dirs, path: &Path) {
    println!("{}", path.strip_prefix(&dirs.root).unwrap_or(path).display());
}

fn run(command: &mut Command) -> Result<()> {
    let output = command.output().with_context(|| format!("failed to start {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} exited with {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn require_tool(tool: &str) -> Result<()> {
    let path = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&path).any(|dir| dir.join(tool).is_file()) {
        return Ok(());
    }
    bail!("{tool} is not on PATH (fontc: cargo install fontc; the others: brew install harfbuzz ttfautohint woff2)")
}
