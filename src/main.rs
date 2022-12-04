#![allow(dead_code)]

use cfg_if::cfg_if;
use clap::Parser;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, UdpSocket},
    runtime,
};

mod cli;
mod dns;
mod dns_client;
mod dns_conf;
mod dns_mw;
mod dns_mw_addr;
mod dns_mw_audit;
mod dns_mw_cache;
mod dns_mw_ns;
mod dns_mw_spdt;
mod dns_mw_zone;
mod dns_server;
mod dns_url;
mod fast_ping;
mod infra;
mod log;
mod matcher;
mod preset_ns;
mod service;
mod third_ext;

use dns_mw::DnsMiddlewareBuilder;
use dns_mw_addr::AddressMiddleware;
use dns_mw_audit::DnsAuditMiddleware;
use dns_mw_cache::DnsCacheMiddleware;
use dns_mw_ns::NameServerMiddleware;
use dns_mw_spdt::DnsSpeedTestMiddleware;
use dns_mw_zone::DnsZoneMiddleware;
use dns_server::{MiddlewareBasedRequestHandler, ServerFuture};
use infra::middleware;
use log::logger;

use crate::log::{debug, error, info};
use crate::{
    dns_client::DnsClient, dns_conf::SmartDnsConfig, matcher::DomainNameServerGroupMatcher,
};

fn banner() {
    info!("");
    info!(r#"     _____                      _       _____  _   _  _____ "#);
    info!(r#"    / ____|                    | |     |  __ \| \ | |/ ____|"#);
    info!(r#"   | (___  _ __ ___   __ _ _ __| |_    | |  | |  \| | (___  "#);
    info!(r#"    \___ \| '_ ` _ \ / _` | '__| __|   | |  | | . ` |\___ \ "#);
    info!(r#"    ____) | | | | | | (_| | |  | |_    | |__| | |\  |____) |"#);
    info!(r#"   |_____/|_| |_| |_|\__,_|_|   \__|   |_____/|_| \_|_____/ "#);
    info!("");
}

/// The app name
const NAME: &'static str = "Smart-DNS";

/// The default configuration.
const DEFAULT_CONF: &'static str = include_str!("../etc/smartdns/smartdns.conf");

/// Returns a version as specified in Cargo.toml
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() {
    use cli::*;
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { conf, debug } => {
            run_server(conf, debug);
        }
        Commands::Service {
            command: service_command,
        } => {
            use service::*;
            use ServiceCommands::*;
            match service_command {
                Install => install(),
                Uninstall{purge} => uninstall(purge),
                Start => start(),
                Stop => stop(),
                Restart => restart(),
                Status => status()
            }
        }
    }
}

fn run_server(conf: Option<PathBuf>, debug: bool) {
    logger(if debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    });

    info!("Smart-DNS 🐋 {} starting", trust_dns_client::version());

    let cfg = if let Some(ref conf) = conf {
        info!("loading configuration from: {:?}", conf);
        SmartDnsConfig::load_from_file(conf.as_path())
    } else {
        cfg_if! {
            if #[cfg(target_os = "android")] {
                let candidate_path = [
                    "/data/data/com.termux/files/usr/etc/smartdns.conf",
                    "/data/data/com.termux/files/usr/etc/smartdns/smartdns.conf"
                ];

            } else if #[cfg(target_os = "windows")] {
                let candidate_path  = [""];
            } else {
                let candidate_path = [
                    "/etc/smartdns.conf",
                    "/etc/smartdns/smartdns.conf",
                    "/usr/local/etc/smartdns.conf",
                    "/usr/local/etc/smartdns/smartdns.conf"
                ];
            }
        };

        candidate_path
            .iter()
            .map(Path::new)
            .filter(|p| p.exists())
            .map(|p| {
                info!("loading configuration from: {:?}", p);
                SmartDnsConfig::load_from_file(p)
            })
            .next()
            .expect("No configuation file found.")
    };

    info!(r#"whoami 👉 "{}""#, cfg.server_name);

    // if !args.debug {
    //     cfg.log_level.as_ref().map(|lvl| {
    //         if let Ok(lvl) = tracing::Level::from_str(lvl) {
    //             logger(lvl);
    //         } else {
    //             warn!("log-level expect: debug,info,warn,error");
    //         }
    //     });
    // }

    let runtime = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_name("smartdns-runtime")
        .build()
        .expect("failed to initialize Tokio Runtime");

    let udp_socket_addrs = cfg.binds.clone().into_iter().map(|s| s.addr).flatten();
    let tcp_socket_addrs = cfg.binds_tcp.clone().into_iter().map(|s| s.addr).flatten();

    // build handle pipeline.
    let middleware = {
        let _guard = runtime.enter();
        let dns_client = Arc::new(DnsClient::new(
            DomainNameServerGroupMatcher::create(&cfg),
            cfg.servers.clone(),
            Default::default(),
        ));

        let mut middleware_builder = DnsMiddlewareBuilder::new();

        // check if audit enabled.
        if cfg.audit_enable && cfg.audit_file.is_some() {
            middleware_builder =
                middleware_builder.with(DnsAuditMiddleware::new(
                    cfg.audit_file.as_ref().unwrap(),
                    cfg.audit_size(),
                    cfg.audit_num()
                ));
        }

        middleware_builder = middleware_builder.with(DnsZoneMiddleware);

        if cfg.address_rules.len() > 0 {
            middleware_builder = middleware_builder.with(AddressMiddleware::new(&cfg));
        }

        // check if cache enabled.
        if cfg.cache_size() > 0 {
            middleware_builder =
                middleware_builder.with(DnsCacheMiddleware::new(&cfg, dns_client.clone()));
        }

        // check if speed_check enabled.
        if !cfg.speed_check_mode.is_empty() {
            middleware_builder = middleware_builder.with(DnsSpeedTestMiddleware);
        }

        middleware_builder = middleware_builder.with(NameServerMiddleware::new(&cfg));

        MiddlewareBasedRequestHandler::new(middleware_builder.build(cfg, dns_client.clone()))
    };

    let mut server = ServerFuture::new(middleware);

    // load udp the listeners
    for udp_socket in udp_socket_addrs {
        debug!("binding UDP to {:?}", udp_socket);
        let udp_socket = runtime
            .block_on(UdpSocket::bind(udp_socket))
            .unwrap_or_else(|_| panic!("could not bind to udp: {}", udp_socket));

        info!(
            "listening for UDP on {:?}",
            udp_socket
                .local_addr()
                .expect("could not lookup local address")
        );

        let _guard = runtime.enter();
        server.register_socket(udp_socket);
    }

    // and TCP as necessary
    for tcp_listener in tcp_socket_addrs {
        info!("binding TCP to {:?}", tcp_listener);
        let tcp_listener = runtime
            .block_on(TcpListener::bind(tcp_listener))
            .unwrap_or_else(|_| panic!("could not bind to tcp: {}", tcp_listener));

        info!(
            "listening for TCP on {:?}",
            tcp_listener
                .local_addr()
                .expect("could not lookup local address")
        );

        let _guard = runtime.enter();
        server.register_listener(tcp_listener, Duration::from_secs(5));
    }

    // config complete, starting!

    banner();
    info!("awaiting connections...");

    info!("Server starting up");
    match runtime.block_on(server.block_until_done()) {
        Ok(()) => {
            // we're exiting for some reason...
            info!("{} {} stopping", NAME, version());
        }
        Err(e) => {
            let error_msg = format!("{} {} has encountered an error: {}", NAME, version(), e);

            error!("{}", error_msg);
            panic!("{}", error_msg);
        }
    };
}


