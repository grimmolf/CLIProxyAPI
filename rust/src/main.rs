use clap::Parser;
use cliproxy_rs::core::{Account, AppState, Provider, Store, UsageRecord};
use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    version,
    about = "Rust multi-account AI proxy with an embedded control room"
)]
struct Args {
    #[arg(long, env = "CLIPROXY_LISTEN", default_value = "127.0.0.1:8318")]
    listen: SocketAddr,
    #[arg(long, env = "CLIPROXY_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "CLIPROXY_API_KEY")]
    api_key: Option<String>,
    #[arg(long, env = "CLIPROXY_MANAGEMENT_TOKEN")]
    management_token: Option<String>,
    #[arg(
        long,
        help = "Show isolated, clearly labeled example accounts and usage; inference is disabled"
    )]
    demo: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cliproxy_rs=info".into()),
        )
        .init();
    let args = Args::parse();
    if args
        .api_key
        .as_ref()
        .is_some_and(|key| key.trim().is_empty())
        || args
            .management_token
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
    {
        return Err("Authentication keys must not be empty".into());
    }
    if !args.listen.ip().is_loopback() && (args.demo || args.api_key.is_none()) {
        return Err(
            "Non-loopback listeners require CLIPROXY_API_KEY; demo mode is loopback-only".into(),
        );
    }
    let data_dir = args.data_dir.unwrap_or_else(|| {
        PathBuf::from(if args.demo {
            ".cliproxy-demo"
        } else {
            ".cliproxy-rs"
        })
    });
    std::fs::create_dir_all(&data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let management_token = match args.management_token {
        Some(token) => token,
        None if args.demo => "demo".into(),
        None => bootstrap_token(&data_dir.join("management-token"))?,
    };
    let store = Store::open(data_dir.join("state.json"))
        .await
        .map_err(|_| "Could not open proxy state; check its permissions and JSON format")?;
    if args.demo && store.accounts().await.is_empty() {
        seed_demo(&store)
            .await
            .map_err(|_| "Could not initialize demo state")?;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let state = AppState {
        store,
        client,
        api_key: args.api_key,
        management_token,
        demo: args.demo,
    };
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!("CLIProxy Rust dashboard: http://{}", listener.local_addr()?);
    if args.demo {
        eprintln!("Demo mode: use management token 'demo'. Example data only; inference disabled.");
    } else {
        eprintln!(
            "Management token: configured via CLIPROXY_MANAGEMENT_TOKEN or stored in {}",
            data_dir.join("management-token").display()
        );
    }
    axum::serve(listener, cliproxy_rs::app(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn bootstrap_token(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    match std::fs::read_to_string(path) {
        Ok(value) if !value.trim().is_empty() => return Ok(value.trim().to_owned()),
        Ok(_) => return Err("Management token file is empty".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let value = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{value}")?;
    file.sync_all()?;
    Ok(value)
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn seed_demo(store: &Store) -> Result<(), cliproxy_rs::core::ApiError> {
    let now = chrono::Utc::now();
    let specs = [
        (Provider::Openai, "Personal workspace", vec!["gpt-4.1"]),
        (Provider::Openai, "Team workspace", vec!["gpt-4.1"]),
        (Provider::Claude, "Research", vec!["claude-sonnet-4-5"]),
        (Provider::Gemini, "Studio", vec!["gemini-2.5-flash"]),
    ];
    let mut accounts = Vec::new();
    for (provider, label, models) in specs {
        let account = Account {
            id: Uuid::new_v4().to_string(),
            label: label.into(),
            provider,
            base_url: provider.default_base_url().into(),
            secret: "demo-placeholder".into(),
            refresh_token: None,
            expires_at: None,
            enabled: true,
            models: models.into_iter().map(str::to_owned).collect(),
            created_at: now.to_rfc3339(),
            auth_kind: "api_key".into(),
            chatgpt_account_id: None,
        };
        store.save_account(account.clone()).await?;
        accounts.push(account);
    }
    for index in 0..36 {
        let account = &accounts[index % accounts.len()];
        let failed = index % 11 == 0;
        store
            .record_usage(UsageRecord {
                id: Uuid::new_v4().to_string(),
                timestamp: (now - chrono::Duration::minutes((index * 83) as i64)).to_rfc3339(),
                account_id: account.id.clone(),
                account_label: account.label.clone(),
                provider: account.provider,
                model: account.models[0].clone(),
                endpoint: if account.provider == Provider::Claude {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                }
                .into(),
                status: if failed { 429 } else { 200 },
                duration_ms: (350 + index * 127) as u64,
                input_tokens: if failed {
                    None
                } else {
                    Some((1200 + index * 97) as u64)
                },
                output_tokens: if failed {
                    None
                } else {
                    Some((240 + index * 31) as u64)
                },
                cached_tokens: if failed {
                    None
                } else {
                    Some((index * 13) as u64)
                },
                reasoning_tokens: None,
                stream: index % 3 != 0,
                error: if failed {
                    Some("Example rate limit".into())
                } else {
                    None
                },
            })
            .await?;
    }
    Ok(())
}
