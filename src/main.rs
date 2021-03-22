#![forbid(unsafe_code)]
#![cfg_attr(feature = "unstable", feature(ip))]
#![recursion_limit = "512"]

extern crate openssl;
#[macro_use]
extern crate rocket;
#[macro_use]
extern crate serde;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate log;
#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

use std::{
    fs::create_dir_all,
    panic,
    path::Path,
    process::exit,
    str::FromStr,
    thread,
};

#[macro_use]
mod error;
mod api;
mod auth;
mod config;
mod crypto;
#[macro_use]
mod db;
mod mail;
mod util;

pub use config::CONFIG;
pub use error::{Error, MapResult};
use rocket::data::{Limits, ToByteUnit};
pub use util::is_running_in_docker;

async fn async_main() -> Result<(), Error> {
    parse_args();
    launch_info();

    use log::LevelFilter as LF;
    let level = LF::from_str(&CONFIG.log_level()).expect("Valid log level");
    init_logging(level).ok();

    let extra_debug = match level {
        LF::Trace | LF::Debug => true,
        _ => false,
    };

    check_data_folder();
    check_rsa_keys().unwrap_or_else(|_| {
        error!("Error creating keys, exiting...");
        exit(1);
    });
    check_web_vault();

    create_icon_cache_folder();

    launch_rocket(extra_debug).await
}

const HELP: &str = "\
        A Bitwarden API server written in Rust
        
        USAGE:
            bitwarden_rs
        
        FLAGS:
            -h, --help       Prints help information
            -v, --version    Prints the app version
";

fn parse_args() {
    const NO_VERSION: &str = "(Version info from Git not present)";
    let mut pargs = pico_args::Arguments::from_env();

    if pargs.contains(["-h", "--help"]) {
        println!("bitwarden_rs {}", option_env!("BWRS_VERSION").unwrap_or(NO_VERSION));
        print!("{}", HELP);
        exit(0);
    } else if pargs.contains(["-v", "--version"]) {
        println!("bitwarden_rs {}", option_env!("BWRS_VERSION").unwrap_or(NO_VERSION));
        exit(0);
    }
}

fn launch_info() {
    println!("/--------------------------------------------------------------------\\");
    println!("|                       Starting Bitwarden_RS                        |");

    if let Some(version) = option_env!("BWRS_VERSION") {
        println!("|{:^68}|", format!("Version {}", version));
    }

    println!("|--------------------------------------------------------------------|");
    println!("| This is an *unofficial* Bitwarden implementation, DO NOT use the   |");
    println!("| official channels to report bugs/features, regardless of client.   |");
    println!("| Send usage/configuration questions or feature requests to:         |");
    println!("|   https://bitwardenrs.discourse.group/                             |");
    println!("| Report suspected bugs/issues in the software itself at:            |");
    println!("|   https://github.com/dani-garcia/bitwarden_rs/issues/new           |");
    println!("\\--------------------------------------------------------------------/\n");
}

fn init_logging(level: log::LevelFilter) -> Result<(), fern::InitError> {
    let mut logger = fern::Dispatch::new()
        .level(level)
        // Hide unknown certificate errors if using self-signed
        .level_for("rustls::session", log::LevelFilter::Off)
        // Hide failed to close stream messages
        .level_for("hyper::server", log::LevelFilter::Warn)
        // Silence rocket logs
        .level_for("_", log::LevelFilter::Off)
        .level_for("launch", log::LevelFilter::Warn)
        .level_for("launch_", log::LevelFilter::Warn)
        .level_for("rocket::rocket", log::LevelFilter::Warn)
        .level_for("rocket::server", log::LevelFilter::Warn)
        .level_for("rocket::fairing::fairings", log::LevelFilter::Warn)
        // Never show html5ever and hyper::proto logs, too noisy
        .level_for("html5ever", log::LevelFilter::Off)
        .level_for("hyper::proto", log::LevelFilter::Off)
        .chain(std::io::stdout());

    // Enable smtp debug logging only specifically for smtp when need.
    // This can contain sensitive information we do not want in the default debug/trace logging.
    if CONFIG.smtp_debug() {
        println!("[WARNING] SMTP Debugging is enabled (SMTP_DEBUG=true). Sensitive information could be disclosed via logs!");
        println!("[WARNING] Only enable SMTP_DEBUG during troubleshooting!\n");
        logger = logger.level_for("lettre::transport::smtp", log::LevelFilter::Debug)
    } else {
        logger = logger.level_for("lettre::transport::smtp", log::LevelFilter::Off)
    }

    if CONFIG.extended_logging() {
        logger = logger.format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                chrono::Local::now().format(&CONFIG.log_timestamp_format()),
                record.target(),
                record.level(),
                message
            ))
        });
    } else {
        logger = logger.format(|out, message, _| out.finish(format_args!("{}", message)));
    }

    if let Some(log_file) = CONFIG.log_file() {
        logger = logger.chain(fern::log_file(log_file)?);
    }

    #[cfg(not(windows))]
    {
        if cfg!(feature = "enable_syslog") || CONFIG.use_syslog() {
            logger = chain_syslog(logger);
        }
    }

    logger.apply()?;

    // Catch panics and log them instead of default output to StdErr
    panic::set_hook(Box::new(|info| {
        let thread = thread::current();
        let thread = thread.name().unwrap_or("unnamed");

        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &**s,
                None => "Box<Any>",
            },
        };

        let backtrace = backtrace::Backtrace::new();

        match info.location() {
            Some(location) => {
                error!(
                    target: "panic", "thread '{}' panicked at '{}': {}:{}\n{:?}",
                    thread,
                    msg,
                    location.file(),
                    location.line(),
                    backtrace
                );
            }
            None => error!(
                target: "panic",
                "thread '{}' panicked at '{}'\n{:?}",
                thread,
                msg,
                backtrace
            ),
        }
    }));

    Ok(())
}

#[cfg(not(windows))]
fn chain_syslog(logger: fern::Dispatch) -> fern::Dispatch {
    let syslog_fmt = syslog::Formatter3164 {
        facility: syslog::Facility::LOG_USER,
        hostname: None,
        process: "bitwarden_rs".into(),
        pid: 0,
    };

    match syslog::unix(syslog_fmt) {
        Ok(sl) => logger.chain(sl),
        Err(e) => {
            error!("Unable to connect to syslog: {:?}", e);
            logger
        }
    }
}

fn create_dir(path: &str, description: &str) {
    // Try to create the specified dir, if it doesn't already exist.
    let err_msg = format!("Error creating {} directory '{}'", description, path);
    create_dir_all(path).expect(&err_msg);
}

fn create_icon_cache_folder() {
    create_dir(&CONFIG.icon_cache_folder(), "icon cache");
}

fn check_data_folder() {
    let data_folder = &CONFIG.data_folder();
    let path = Path::new(data_folder);
    if !path.exists() {
        error!("Data folder '{}' doesn't exist.", data_folder);
        if is_running_in_docker() {
            error!("Verify that your data volume is mounted at the correct location.");
        } else {
            error!("Create the data folder and try again.");
        }
        exit(1);
    }
}

fn check_rsa_keys() -> Result<(), crate::error::Error> {
    // If the RSA keys don't exist, try to create them
    let priv_path = CONFIG.private_rsa_key();
    let pub_path = CONFIG.public_rsa_key();

    if !util::file_exists(&priv_path) {
        let rsa_key = openssl::rsa::Rsa::generate(2048)?;

        let priv_key = rsa_key.private_key_to_pem()?;
        crate::util::write_file(&priv_path, &priv_key)?;
        info!("Private key created correctly.");
    }

    if !util::file_exists(&pub_path) {
        let rsa_key = openssl::rsa::Rsa::private_key_from_pem(&util::read_file(&priv_path)?)?;

        let pub_key = rsa_key.public_key_to_pem()?;
        crate::util::write_file(&pub_path, &pub_key)?;
        info!("Public key created correctly.");
    }

    auth::load_keys();
    Ok(())
}

fn check_web_vault() {
    if !CONFIG.web_vault_enabled() {
        return;
    }

    let index_path = Path::new(&CONFIG.web_vault_folder()).join("index.html");

    if !index_path.exists() {
        error!("Web vault is not found at '{}'. To install it, please follow the steps in: ", CONFIG.web_vault_folder());
        error!("https://github.com/dani-garcia/bitwarden_rs/wiki/Building-binary#install-the-web-vault");
        error!("You can also set the environment variable 'WEB_VAULT_ENABLED=false' to disable it");
        exit(1);
    }
}

async fn launch_rocket(extra_debug: bool) -> Result<(), Error> {
    let pool = match util::retry_db(db::DbPool::from_config, CONFIG.db_connection_retries()) {
        Ok(p) => p,
        Err(e) => {
            error!("Error creating database pool: {:?}", e);
            exit(1);
        }
    };

    api::start_send_deletion_scheduler(pool.clone());

    let basepath = &CONFIG.domain_path();

    let mut config = rocket::Config::from(rocket::Config::figment());
    config.address = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

    config.limits = Limits::new()
        .limit("json", 10.megabytes())
        .limit("data-form", 150.megabytes())
        .limit("file", 150.megabytes());

    // If adding more paths here, consider also adding them to
    // crate::utils::LOGGED_ROUTES to make sure they appear in the log
    let instance = rocket::custom(config)
        .mount(&[basepath, "/"].concat(), api::web_routes())
        .mount(&[basepath, "/api"].concat(), api::core_routes())
        .mount(&[basepath, "/admin"].concat(), api::admin_routes())
        .mount(&[basepath, "/identity"].concat(), api::identity_routes())
        .mount(&[basepath, "/icons"].concat(), api::icons_routes())
        .mount(&[basepath, "/notifications"].concat(), api::notifications_routes())
        .manage(pool)
        .manage(api::start_notification_server())
        .attach(util::AppHeaders())
        .attach(util::CORS())
        .attach(util::BetterLogging(extra_debug));

    CONFIG.set_rocket_shutdown_handle(instance.shutdown());
    ctrlc::set_handler(move || {
        info!("Exiting bitwarden_rs!");
        CONFIG.shutdown();
    })
    .expect("Error setting Ctrl-C handler");
    
    instance.launch().await?;
    
    info!("Bitwarden_rs process exited!");
    Ok(())
}

fn main() -> Result<(), Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}
