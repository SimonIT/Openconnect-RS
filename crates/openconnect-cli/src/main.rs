mod cli;
mod daemon;
mod sock;

use crate::sock::Server;
use clap::Parser;
use cli::{Cli, Commands, ServerType};
use comfy_table::Table;
use futures::{SinkExt, TryStreamExt};
use openconnect_core::{
    config::{ConfigBuilder, EntrypointBuilder, LogLevel},
    events::EventHandlers,
    ip_info::IpInfo,
    log::Logger,
    storage::{OidcServer, PasswordServer, StoredConfigs, StoredServer},
    Connectable, Status, VpnClient,
};
use std::{error::Error, io::BufRead, path::PathBuf, sync::Arc};
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};

#[derive(serde::Serialize, serde::Deserialize)]
pub enum JsonRequest {
    Stop,
    Info,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub enum JsonResponse {
    StopResult {
        server_name: String,
    },
    InfoResult {
        server_name: String,
        server_url: String,
        hostname: String,
        status: String,
        info: Option<Box<IpInfo>>,
    },
}

async fn connect_password_server(
    password_server: &PasswordServer,
    stored_configs: &StoredConfigs,
) -> Result<Arc<VpnClient>, Box<dyn Error>> {
    let password_server = password_server.decrypted_by(&stored_configs.cipher);

    let config = ConfigBuilder::default()
        .vpncscript("/opt/vpnc-scripts/vpnc-script")
        .loglevel(LogLevel::Info)
        .build()?;

    let entrypoint = EntrypointBuilder::new()
        .name(&password_server.name)
        .server(&password_server.server)
        .username(&password_server.username)
        .password(&password_server.password.clone().unwrap_or("".to_string()))
        .accept_insecure_cert(password_server.allow_insecure.unwrap_or(false))
        .enable_udp(true)
        .build()?;

    let event_handler = EventHandlers::default();

    let client = VpnClient::new(config, event_handler)?;
    let client_clone = client.clone();

    tokio::task::spawn_blocking(move || {
        let _ = client_clone.connect(entrypoint);
    });

    Ok(client)
}

async fn try_accept(listener: &tokio::net::UnixListener, client: Arc<VpnClient>) {
    if let Ok((stream, _)) = listener.accept().await {
        let (read, write) = stream.into_split();
        let mut framed_reader = sock::get_framed_reader::<JsonRequest>(read);
        let mut framed_writer = sock::get_framed_writer::<JsonResponse>(write);

        tokio::spawn(async move {
            while let Ok(Some(command)) = framed_reader.try_next().await {
                match command {
                    JsonRequest::Stop => {
                        let server_name = client.get_server_name().unwrap_or("".to_string());
                        client.disconnect();

                        // ignore send error
                        let _ = framed_writer
                            .send(JsonResponse::StopResult { server_name })
                            .await;
                        unsafe {
                            libc::raise(libc::SIGTERM);
                        }
                    }

                    JsonRequest::Info => {
                        let server_name = client.get_server_name().unwrap_or("".to_string());
                        let server_url = client.get_server_url().unwrap_or("".to_string());
                        let hostname = client.get_hostname().unwrap_or("".to_string());
                        let status = client.get_status();
                        let info = client.get_info().ok().flatten().map(Box::new);
                        let status = match status {
                            Status::Connected => "Connected",
                            Status::Connecting(_) => "Connecting",
                            Status::Disconnected => "Disconnected",
                            Status::Disconnecting => "Disconnecting",
                            Status::Error(_) => "Error",
                            Status::Initialized => "Initialized",
                        }
                        .to_string();

                        // ignore send error
                        let _ = framed_writer
                            .send(JsonResponse::InfoResult {
                                server_name,
                                server_url,
                                hostname,
                                status,
                                info,
                            })
                            .await;
                    }
                }
            }
        });
    }
}

async fn start_daemon(
    stored_server: &StoredServer,
    stored_configs: &StoredConfigs,
) -> Result<(), Box<dyn Error>> {
    let server = Server::bind()?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigquit = signal(SignalKind::quit())?;

    let client = match stored_server {
        StoredServer::Password(password_server) => {
            connect_password_server(password_server, stored_configs).await?
        }
        StoredServer::Oidc(_) => {
            panic!("OIDC server not implemented");
        }
    };

    loop {
        select! {
            _ = sigquit.recv() => {
                break;
            }
            _ = sigint.recv() => {
                break;
            }
            _ = sigterm.recv() => {
                break;
            }
            _ = try_accept(&server.listener, client.clone()) => {

            }
        };
    }

    Ok(())
}

async fn get_server(
    server_name: &str,
    config_file: PathBuf,
) -> Result<(StoredServer, StoredConfigs), Box<dyn Error>> {
    let mut stored_configs = StoredConfigs::new(None, config_file);
    let config = stored_configs.read_from_file().await?;
    let server = config.servers.get(server_name);

    match server {
        Some(server) => {
            match server {
                StoredServer::Oidc(OidcServer { server, .. }) => {
                    println!("Connecting to OIDC server: {}", server_name);
                    println!("Server host: {}", server);
                }
                StoredServer::Password(PasswordServer { server, .. }) => {
                    println!("Connecting to password server: {}", server_name);
                    println!("Server host: {}", server);
                }
            }
            Ok((server.clone(), stored_configs))
        }
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Server {} not found", server_name),
        ))?,
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Add(server_type) => {
            let new_server = match server_type {
                ServerType::Oidc {
                    name,
                    server,
                    issuer,
                    client_id,
                    client_secret,
                    allow_insecure,
                } => {
                    let oidc_server = OidcServer {
                        name,
                        server,
                        issuer,
                        client_id,
                        client_secret,
                        allow_insecure,
                        updated_at: None,
                    };

                    StoredServer::Oidc(oidc_server)
                }
                ServerType::Password {
                    name,
                    server,
                    username,
                    password,
                    allow_insecure,
                } => {
                    let password_server = PasswordServer {
                        name,
                        server,
                        username,
                        password: Some(password),
                        allow_insecure,
                        updated_at: None,
                    };

                    StoredServer::Password(password_server)
                }
            };

            let config_file =
                StoredConfigs::getorinit_config_file().expect("Failed to get config file");

            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            runtime.block_on(async {
                let mut stored_configs = StoredConfigs::new(None, config_file);

                stored_configs
                    .read_from_file()
                    .await
                    .expect("Failed to read config file");

                stored_configs
                    .upsert_server(new_server)
                    .await
                    .expect("Failed to add server");
            });
        }

        Commands::Delete { name } => {
            let config_file =
                StoredConfigs::getorinit_config_file().expect("Failed to get config file");

            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            runtime.block_on(async {
                let mut stored_configs = StoredConfigs::new(None, config_file);

                stored_configs
                    .read_from_file()
                    .await
                    .expect("Failed to read config file");

                stored_configs
                    .remove_server(&name)
                    .await
                    .expect("Failed to delete server");
            });
        }

        Commands::List => {
            let config_file =
                StoredConfigs::getorinit_config_file().expect("Failed to get config file");

            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            runtime.block_on(async {
                let mut stored_configs = StoredConfigs::new(None, config_file);

                let stored_configs = stored_configs.read_from_file().await.unwrap();
                let mut table = Table::new();
                table.set_header(vec![
                    "Name".to_string(),
                    "Type".to_string(),
                    "Server".to_string(),
                    "Allow Insecure".to_string(),
                    "Updated At".to_string(),
                ]);

                for (name, server) in stored_configs.servers.iter() {
                    match server {
                        StoredServer::Oidc(OidcServer {
                            server,
                            allow_insecure,
                            updated_at,
                            ..
                        }) => {
                            table.add_row(vec![
                                name.clone(),
                                "OIDC Server".to_string(),
                                server.clone(),
                                allow_insecure.unwrap_or(false).to_string(),
                                updated_at.as_ref().unwrap_or(&"".to_string()).to_owned(),
                            ]);
                        }
                        StoredServer::Password(PasswordServer {
                            server,
                            allow_insecure,
                            updated_at,
                            ..
                        }) => {
                            table.add_row(vec![
                                name.clone(),
                                "Password Server".to_string(),
                                server.clone(),
                                allow_insecure.unwrap_or(false).to_string(),
                                updated_at.as_ref().unwrap_or(&"".to_string()).to_owned(),
                            ]);
                        }
                    }
                }

                println!("{table}");
            });
        }

        Commands::Status => {
            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");

            runtime.block_on(async {
                let client = sock::Client::connect().await;

                match client {
                    Ok(mut client) => {
                        client
                            .send(JsonRequest::Info)
                            .await
                            .expect("Failed to send info command");

                        if let Ok(Some(response)) = client.framed_reader.try_next().await {
                            match response {
                                JsonResponse::InfoResult {
                                    server_name,
                                    server_url,
                                    hostname,
                                    status,
                                    info,
                                } => {
                                    let mut table = Table::new();
                                    let mut rows = vec![
                                        vec![format!("Server Name"), server_name],
                                        vec![format!("Server URL"), server_url],
                                        vec![format!("Server IP"), hostname],
                                        vec![format!("Connection Status"), status],
                                    ];

                                    if let Some(info) = info {
                                        let addr = info.addr.unwrap_or("".to_string());
                                        let netmask = info.netmask.unwrap_or("".to_string());
                                        let addr6 = info.addr6.unwrap_or("".to_string());
                                        let netmask6 = info.netmask6.unwrap_or("".to_string());
                                        let dns1 = info.dns[0].clone().unwrap_or("".to_string());
                                        let dns2 = info.dns[1].clone().unwrap_or("".to_string());
                                        let dns3 = info.dns[2].clone().unwrap_or("".to_string());
                                        let nbns1 = info.nbns[0].clone().unwrap_or("".to_string());
                                        let nbns2 = info.nbns[1].clone().unwrap_or("".to_string());
                                        let nbns3 = info.nbns[2].clone().unwrap_or("".to_string());
                                        let domain = info.domain.unwrap_or("".to_string());
                                        let proxy_pac = info.proxy_pac.unwrap_or("".to_string());
                                        let mtu = info.mtu.to_string();
                                        let gateway_addr =
                                            info.gateway_addr.clone().unwrap_or("".to_string());
                                        let info_rows = vec![
                                            vec![format!("IPv4 Address"), addr],
                                            vec![format!("IPv4 Netmask"), netmask],
                                            vec![format!("IPv6 Address"), addr6],
                                            vec![format!("IPv6 Netmask"), netmask6],
                                            vec![format!("DNS 1"), dns1],
                                            vec![format!("DNS 2"), dns2],
                                            vec![format!("DNS 3"), dns3],
                                            vec![format!("NBNS 1"), nbns1],
                                            vec![format!("NBNS 2"), nbns2],
                                            vec![format!("NBNS 3"), nbns3],
                                            vec![format!("Domain"), domain],
                                            vec![format!("Proxy PAC"), proxy_pac],
                                            vec![format!("MTU"), mtu],
                                            vec![format!("Gateway Address"), gateway_addr],
                                        ];

                                        rows.extend(info_rows);
                                    }

                                    table.add_rows(rows);

                                    println!("{table}");
                                }
                                _ => {
                                    println!("Received unexpected response");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("Failed to connect to server: {}", e);
                    }
                }
            });
        }

        Commands::Logs => {
            let log_path = Logger::get_log_path();
            let files = std::fs::read_dir(log_path)
                .expect("Failed to read log directory")
                .flatten()
                .filter(|f| f.metadata().unwrap().is_file())
                .max_by_key(|f| f.metadata().unwrap().modified().unwrap());

            if let Some(file) = files {
                let file = std::fs::File::open(file.path()).expect("Failed to open log file");
                let reader = std::io::BufReader::new(file);
                for line in reader.lines() {
                    println!("{}", line.unwrap());
                }
            }
        }

        Commands::Stop => {
            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");

            runtime.block_on(async {
                let client = sock::Client::connect().await;

                match client {
                    Ok(mut client) => {
                        client
                            .send(JsonRequest::Stop)
                            .await
                            .expect("Failed to send stop command");

                        if let Ok(Some(response)) = client.framed_reader.try_next().await {
                            match response {
                                JsonResponse::StopResult { server_name } => {
                                    println!("Stopped connection to server: {}", server_name)
                                }
                                _ => {
                                    println!("Received unexpected response");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("Failed to connect to server: {}", e);
                    }
                };
            });
        }

        Commands::Start { name, config_file } => {
            if sock::exists() {
                println!("Socket already exists. You may have a connected VPN session or a stale socket file. You may solve by:");
                println!("1. Stopping the connection by sending stop command.");
                println!(
                    "2. Manually deleting the socket file which located at: {}",
                    sock::get_sock().display()
                );
                std::process::exit(1);
            }

            let config_file = config_file.map(PathBuf::from).unwrap_or(
                StoredConfigs::getorinit_config_file().expect("Failed to get config file"),
            );

            sudo::escalate_if_needed().expect("Failed to escalate permissions");
            match daemon::daemonize() {
                daemon::ForkResult::Parent => {
                    let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");
                    runtime.block_on(async {
                        match get_server(&name, config_file).await {
                            Ok(_) => {}
                            Err(e) => {
                                println!("Failed to get server: {}", e);
                                std::process::exit(1);
                            }
                        }
                    });
                    println!("The process will be running in the background, you should use cli to interact with it.");
                    std::process::exit(0);
                }
                daemon::ForkResult::Child => {
                    std::process::exit(0);
                }
                daemon::ForkResult::Grandchild => {
                    // Daemon process
                }
            }

            let runtime = tokio::runtime::Runtime::new().expect("Failed to create runtime");

            let (server, configs) = runtime.block_on(async {
                get_server(&name, config_file)
                    .await
                    .expect("Failed to get server")
            });

            runtime.block_on(async {
                Logger::init().expect("Failed to initialize logger");
                let _ = start_daemon(&server, &configs).await.inspect_err(|e| {
                    tracing::error!("Failed to start daemon: {}", e);
                });
            });
        }
    }
}
