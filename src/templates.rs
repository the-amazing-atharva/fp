use crate::config::api_client_configuration;
use anyhow::{anyhow, Context, Error, Result};
use clap::{Parser, ValueHint};
use fiberplane::protocols::core::{self, Cell, HeadingCell, HeadingType, TextCell, TimeRange};
use fiberplane_templates::{notebook_to_template, TemplateExpander};
use fp_api_client::apis::default_api::{get_notebook, notebook_create, proxy_data_sources_list};
use fp_api_client::models::{NewNotebook, Notebook};
use lazy_static::lazy_static;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::{env::current_dir, ffi::OsStr, path::PathBuf, str::FromStr};
use tokio::fs;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

lazy_static! {
    static ref NOTEBOOK_ID_REGEX: Regex = Regex::from_str("([a-zA-Z0-9_-]{22})$").unwrap();
}

// TODO remove these once the relay schema matches the generated API client
use serde::{Deserialize, Serialize};
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DataSourceType {
    Prometheus,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ProxyDataSource {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: DataSourceType,
    pub proxy: ProxySummary,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySummary {
    pub id: String,
    pub name: String,
}

#[derive(Parser)]
pub struct Arguments {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

pub async fn handle_command(args: Arguments) -> Result<()> {
    use SubCommand::*;
    match args.subcmd {
        Init => handle_init_command().await,
        Expand(args) => handle_expand_command(args).await,
        Convert(args) => handle_convert_command(args).await,
    }
}

#[derive(Parser)]
enum SubCommand {
    #[clap(
        name = "init",
        about = "Create a blank template and save it in the current directory as template.jsonnet"
    )]
    Init,

    #[clap(
        name = "expand",
        about = "Expand a template into a Fiberplane notebook"
    )]
    Expand(ExpandArguments),

    #[clap(
        name = "convert",
        about = "Create a template from an existing Fiberplane notebook"
    )]
    Convert(ConvertArguments),
}

#[derive(Parser)]
struct ExpandArguments {
    /// Values to inject into the template. Must be in the form name=value. JSON values are supported.
    #[clap(name = "arg", short, long)]
    args: Vec<TemplateArg>,

    /// Path or URL of template file to expand
    #[clap(value_hint = ValueHint::AnyPath)]
    template: String,

    /// Create the notebook on Fiberplane.com and return the URL
    #[clap(long)]
    create_notebook: bool,

    #[clap(from_global)]
    base_url: String,

    #[clap(from_global)]
    config: Option<String>,
}

#[derive(Parser)]
struct ConvertArguments {
    #[clap(from_global)]
    base_url: String,

    #[clap(from_global)]
    config: Option<String>,

    /// If specified, save the template to the given file. If not, write the template to stdout
    #[clap(long, short)]
    out: Option<PathBuf>,

    /// Notebook URL to convert. Pass \"-\" to read the Notebook JSON representation from stdin
    #[clap()]
    notebook_url: String,
}

pub struct TemplateArg {
    pub name: String,
    pub value: Value,
}

impl FromStr for TemplateArg {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if let Some((name, value)) = s.split_once('=') {
            Ok(TemplateArg {
                name: name.to_string(),
                value: serde_json::from_str(value)
                    .unwrap_or_else(|_| Value::String(value.to_string())),
            })
        } else {
            Err(anyhow!(
                "Invalid argument syntax. Must be in the form name=value"
            ))
        }
    }
}

async fn handle_init_command() -> Result<()> {
    let notebook = core::NewNotebook {
        title: "Replace me!".to_string(),
        time_range: TimeRange {
            from: 0.0,
            to: 60.0 * 60.0,
        },
        data_sources: BTreeMap::new(),
        cells: vec![
            Cell::Heading(HeadingCell {
                id: "1".to_string(),
                heading_type: HeadingType::H1,
                content: "This is a section".to_string(),
                read_only: None,
            }),
            Cell::Text(TextCell {
                id: "2".to_string(),
                content: "You can add any types of cells and pre-fill content".to_string(),
                read_only: None,
            }),
        ],
        labels: Vec::new(),
    };
    let template = notebook_to_template(notebook);

    let mut path = current_dir()?;
    path.push("template.jsonnet");

    fs::write(&path, template).await?;
    eprintln!("Saved template to: {}", path.display());

    Ok(())
}

/// Load the template file, either from a server if the
/// template_path is an HTTPS URL, or from a local file
async fn load_template(template_path: &str) -> Result<String> {
    if template_path.starts_with("https://") || template_path.starts_with("http://") {
        if template_path.starts_with("http://") {
            warn!("Templates can be manually expanded from HTTP URLs but triggers must use HTTPS URLs");
        }
        reqwest::get(template_path)
            .await
            .with_context(|| format!("loading template from URL: {}", template_path))?
            .error_for_status()
            .with_context(|| format!("loading template from URL: {}", template_path))?
            .text()
            .await
            .with_context(|| format!("reading remote file as text: {}", template_path))
    } else {
        let path = PathBuf::from(template_path);
        if path.extension() == Some(OsStr::new("jsonnet")) {
            fs::read_to_string(path)
                .await
                .with_context(|| "reading jsonnet file")
        } else {
            Err(anyhow!("Template must be a .jsonnet file"))
        }
    }
}

async fn handle_expand_command(args: ExpandArguments) -> Result<()> {
    let template = load_template(&args.template).await?;
    let template_args: HashMap<String, Value> =
        args.args.into_iter().map(|a| (a.name, a.value)).collect();

    let config = api_client_configuration(args.config.as_deref(), &args.base_url)
        .await
        .ok();

    let mut expander = TemplateExpander::default();

    // Inject data sources into the template runtime
    let data_sources = if let Some(config) = &config {
        proxy_data_sources_list(config)
            .await
            .with_context(|| "loading proxy data sources")?
    } else {
        Vec::new()
    };
    expander.add_ext_var(
        "PROXY_DATA_SOURCES".to_string(),
        serde_json::to_value(&data_sources)?,
    );

    let notebook = expander
        .expand_template_to_string(template, template_args, !args.create_notebook)
        .with_context(|| "expanding template")?;

    if !args.create_notebook {
        io::stdout().write_all(notebook.as_bytes()).await?;
    } else {
        debug!(%notebook, "Expanded template to notebook");

        let config = config.ok_or_else(|| anyhow!("Must be logged in to create notebook"))?;

        let notebook: NewNotebook = serde_json::from_str(&notebook)
            .with_context(|| "Template did not produce a valid NewNotebook")?;
        let notebook = notebook_create(&config, Some(notebook))
            .await
            .with_context(|| "Error creating notebook")?;
        let notebook_url = format!("{}/notebook/{}", config.base_path, notebook.id);
        eprintln!("Created notebook: {}", notebook_url);
    }
    Ok(())
}

async fn handle_convert_command(args: ConvertArguments) -> Result<()> {
    let (notebook, notebook_id, url) = if args.notebook_url == "-" {
        let mut notebook_json = String::new();
        io::stdin()
            .read_to_string(&mut notebook_json)
            .await
            .with_context(|| "Error reading from stdin")?;
        let notebook: Notebook =
            serde_json::from_str(&notebook_json).with_context(|| "Notebook is invalid")?;
        let url = format!("{}/notebook/{}", args.base_url, &notebook.id);
        (notebook_json, notebook.id, url)
    } else {
        let config = api_client_configuration(args.config.as_deref(), &args.base_url).await?;
        let id = &NOTEBOOK_ID_REGEX
            .captures(&args.notebook_url)
            .ok_or_else(|| anyhow!("Notebook URL is invalid"))?[1];
        let notebook = get_notebook(&config, id)
            .await
            .with_context(|| "Error fetching notebook")?;
        let notebook = serde_json::to_string(&notebook)?;
        (notebook, id.to_string(), args.notebook_url)
    };

    // TODO remove the extra (de)serialization when we unify the generated API client
    // types with those in fiberplane-rs
    let mut notebook: core::NewNotebook = serde_json::from_str(&notebook).with_context(|| {
        format!(
            "Error deserializing response as core::NewNotebook: {}",
            notebook
        )
    })?;

    // Add image URLs to ImageCells that were uploaded to the Studio.
    //
    // Images will be loaded from the API when the notebook is created so
    // that the images are stored as files associated with the new notebook.
    for cell in &mut notebook.cells {
        if let Cell::Image(cell) = cell {
            if let (None, Some(file_id)) = (&cell.url, &cell.file_id) {
                cell.url = Some(format!(
                    "{}/api/files/{}/{}",
                    args.base_url, notebook_id, file_id
                ));
                cell.file_id = None;
            }
        }
    }

    let template = notebook_to_template(notebook);
    let template = format!(
        "
// This template was generated from the notebook: {}

{}",
        url, template
    );
    if let Some(mut path) = args.out {
        // If the given path is a directory, add the filename
        if path.file_name().is_none() {
            path.push("template.jsonnet");
        }

        fs::write(path, template).await?;
    } else {
        io::stdout().write_all(template.as_bytes()).await?;
    }

    Ok(())
}