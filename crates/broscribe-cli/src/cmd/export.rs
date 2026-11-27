//! `export <meeting> [--include …] [--format md|txt|json] [--no-timestamps]
//! [--out <file>|-]` (design §11): the transcript and generated content of
//! one meeting as one document, names filled in and stale sections marked.
//! Without `--out` (or with `-`) the document goes to stdout; a file is
//! written atomically with mode 0600 (it holds a transcript). Sections the
//! meeting does not have are left out and listed in `omitted`.

use std::path::{Path, PathBuf};

use broscribe_core::export::{ExportFormat, outside_library_parent, write_export};
use broscribe_core::{Error, Result};
use serde_json::json;

use super::meeting_id;
use crate::Ctx;
use crate::cli::{ExportArgs, ExportFormatArg};
use crate::query::export_doc;

fn export_format(f: ExportFormatArg) -> ExportFormat {
    match f {
        ExportFormatArg::Md => ExportFormat::Markdown,
        ExportFormatArg::Txt => ExportFormat::Text,
        ExportFormatArg::Json => ExportFormat::Json,
    }
}

/// MCP file writers share the same destination policy.
pub(super) fn mcp_export_path(
    data_dir: &Path,
    library: &Path,
    path: &Path,
    extension: &str,
) -> Result<PathBuf> {
    let destination_error = || {
        Error::InvalidInput(format!(
            "Choose a new .{extension} file outside the library and data folder. Hidden files and stdout are unavailable."
        ))
    };
    let name = path
        .file_name()
        .filter(|name| {
            let bytes = name.as_encoded_bytes();
            !bytes.starts_with(b".") && !bytes.contains(&b':')
        })
        .ok_or_else(destination_error)?;
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    {
        return Err(destination_error());
    }
    let data_dir = data_dir
        .canonicalize()
        .map_err(|e| Error::io("resolving the BroScribe data folder", e))?;
    let parent = outside_library_parent(library, path)
        .filter(|parent| !parent.starts_with(&data_dir))
        .ok_or_else(destination_error)?;
    Ok(parent.join(name))
}

pub(super) fn run(ctx: &mut Ctx<'_>, a: ExportArgs) -> Result<()> {
    let id = meeting_id(&a.meeting)?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let format = export_format(a.format);
    let mcp_destination = if ctx.request_library.is_some() {
        let path = a.out.as_deref().ok_or_else(|| {
            Error::InvalidInput("Choose a destination file for the export.".into())
        })?;
        Some(mcp_export_path(
            ctx.data_dir(),
            lib.root(),
            path,
            format.extension(),
        )?)
    } else {
        None
    };
    let doc = export_doc(&lib, &id, &a.include, format, !a.no_timestamps)?;
    for s in &doc.omitted {
        ctx.note(format!("note: {id} has no {s}; left out"));
    }
    match a.out.filter(|p| p.as_os_str() != "-") {
        Some(path) => {
            let destination = if let Some(destination) = mcp_destination {
                destination
            } else {
                outside_library_parent(lib.root(), &path)
                    .zip(path.file_name())
                    .map(|(parent, name)| parent.join(name))
                    .ok_or_else(|| {
                        Error::InvalidInput(
                            "Choose an existing folder outside the BroScribe library.".into(),
                        )
                    })?
            };
            if ctx.request_cancel.is_some() {
                ctx.check_request_cancelled()?;
                let parent = destination.parent().ok_or_else(|| {
                    Error::InvalidInput("Choose an existing destination folder.".into())
                })?;
                super::write_new_private_file(parent, &destination, doc.content.as_bytes())?;
            } else {
                write_export(&destination, &doc.content)?;
            }
            ctx.emit(
                json!({ "path": path, "sections": doc.sections, "omitted": doc.omitted }),
                |out| writeln!(out, "Exported {id} to {}", path.display()),
            )
        }
        None => ctx.emit(
            json!({ "content": doc.content, "sections": doc.sections, "omitted": doc.omitted }),
            |out| {
                write!(out, "{}", doc.content)?;
                if !doc.content.ends_with('\n') {
                    writeln!(out)?;
                }
                Ok(())
            },
        ),
    }
}
