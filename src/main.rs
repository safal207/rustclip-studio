use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rustclip_studio::{
    graph,
    models::*,
    money,
    pipeline::{self, PipelineConfig},
    publish::{self, PublishRequest},
    render, server,
    store::Store,
    trends,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rustclip",
    version,
    about = "Локальная студия роликов с причинным графом"
)]
struct Cli {
    #[arg(long, global = true, env = "RUSTCLIP_DATA", default_value = "data")]
    data: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Локальный интерфейс: http://127.0.0.1:3939
    Serve {
        #[arg(long, default_value_t = 3939)]
        port: u16,
    },
    /// Доступность бесплатных инструментов и подключений (без секретов)
    Doctor,
    /// Создать и отрендерить оригинальный демонстрационный ролик
    Demo {
        #[arg(long)]
        silent: bool,
    },
    /// Создать проект из JSON и собрать MP4
    Render { spec: PathBuf },
    /// Свежие популярные запросы или ролики
    Trends {
        #[arg(long, default_value = "google")]
        source: String,
        #[arg(long, default_value = "RU")]
        region: String,
    },
    /// Публикация: по умолчанию только локальная симуляция и пакет файлов
    Publish {
        project: String,
        #[arg(long, default_value = "export")]
        platform: String,
        #[arg(long, default_value = "private")]
        privacy: String,
        #[arg(long)]
        live: bool,
    },
    /// Проверить и экспортировать причинный журнал; чтение не запускает действий
    Graph {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Импорт дневных просмотров и доходов из CSV
    ImportLedger { file: PathBuf },
    /// Автоконвейер: тренд → сценарий → рендер → публикация/экспорт
    Pipeline {
        config: PathBuf,
        #[arg(long)]
        watch: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rustclip_studio=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let store = Store::open(&cli.data)?;
    let command = cli.command.unwrap_or(Command::Serve { port: 3939 });
    if matches!(
        command,
        Command::Serve { .. }
            | Command::Demo { .. }
            | Command::Render { .. }
            | Command::Publish { .. }
            | Command::Pipeline { .. }
    ) {
        store.recover_interrupted()?;
    }
    match command {
        Command::Serve { port } => {
            server::serve(store, port).await?;
        }
        Command::Doctor => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"renderer":render::capabilities(),"publish":publish::capabilities(),"graph":graph::verify(&store.events()?)})
                )?
            );
        }
        Command::Demo { silent } => {
            let mut spec = rustclip_studio::preferred_example();
            if silent {
                spec.voice = Voice::None;
            }
            render_one(&store, spec).await?;
        }
        Command::Render { spec } => {
            let bytes = std::fs::read(&spec).context("Не удалось прочитать сценарий")?;
            render_one(&store, serde_json::from_slice(&bytes)?).await?;
        }
        Command::Trends { source, region } => {
            let data = trends::fetch(&source, &region).await?;
            store.save_trends(&data)?;
            println!("{}", serde_json::to_string_pretty(&data)?);
        }
        Command::Publish {
            project,
            platform,
            privacy,
            live,
        } => {
            let request: PublishRequest = serde_json::from_value(
                serde_json::json!({"platform":platform,"privacy":privacy,"dry_run":!live}),
            )?;
            request.validate()?;
            let job = store.queue_publish(&project, serde_json::to_value(&request)?, None)?;
            if ["queued", "scheduled"].contains(&job.state.as_str()) {
                server::execute_publish(&store, &job.id).await?;
            }
            println!("{}", serde_json::to_string_pretty(&store.job(&job.id)?)?);
        }
        Command::Graph {
            project,
            as_of,
            output,
        } => {
            let value = graph::view(&store.events()?, project.as_deref(), as_of.as_deref())?;
            let text = serde_json::to_string_pretty(&value)?;
            if let Some(path) = output {
                std::fs::write(&path, &text)?;
                println!("{}", path.display());
            } else {
                println!("{text}");
            }
        }
        Command::ImportLedger { file } => {
            let rows = money::parse_csv(&std::fs::read(file)?)?;
            store.save_ledger(&rows)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&money::totals(&store.ledger()?)?)?
            );
        }
        Command::Pipeline { config, watch } => {
            let config: PipelineConfig = serde_json::from_slice(&std::fs::read(config)?)?;
            config.validate()?;
            if watch {
                pipeline::watch(store, config).await?;
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&pipeline::cycle(&store, &config).await?)?
                );
            }
        }
    }
    Ok(())
}
async fn render_one(store: &Store, spec: ProjectSpec) -> Result<()> {
    let p = store.create_project(spec, "manual")?;
    let (job, _) = store.start_render(&p.id)?;
    store.begin_job(&job.id)?;
    let result = render::render(&store.project(&p.id)?, &store.root).await;
    let job = store.finish_render(&job.id, result)?;
    if job.state != "succeeded" {
        anyhow::bail!("{}", job.error.unwrap_or_default());
    }
    println!("{}", serde_json::to_string_pretty(&store.project(&p.id)?)?);
    Ok(())
}
