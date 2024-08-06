use chrono::{DateTime, Local};
use clap::Parser;
use fantoccini::elements::Element;
use fantoccini::Client;
use fantoccini::{wd::Capabilities, ClientBuilder, Locator};
use serde::Serialize;
use std::fs::File;
use std::path::PathBuf;
use std::{
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use tokio::signal;
use tracing::{info, warn};
use url::Url;

const DRIVER_PORT: u16 = 9515;
const TEAM_NAME: &str = "Sparta Praha";
const TEAM_URL: &str = "https://www.livesport.cz/tym/sparta-praha/zcG9U7N6/";

#[derive(Debug, Serialize)]
enum GameTime {
    Played,
    BreakAfter(u32),
    Playing(u32),
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct GameResult {
    my_team: String,
    my_team_score: u32,
    opponent_team: String,
    opponent_team_score: u32,
    game_time: GameTime,
    generated: DateTime<Local>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// JSON output file
    output: PathBuf,

    /// Refresh interval
    #[arg(short, long, default_value_t = 30)]
    refresh: u64,
}

fn start_driver() -> anyhow::Result<Child> {
    let driver = Command::new("chromedriver")
        .arg(format!("--port={}", DRIVER_PORT))
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;

    thread::sleep(Duration::from_millis(300));
    Ok(driver)
}

const PERIOD_MINUTES: u32 = 20;

async fn get_minute_of_game(row: &Element) -> anyhow::Result<GameTime> {
    let event_parts = row.find_all(Locator::Css(".event__part--home")).await?;
    let mut periods = 0;
    for part in event_parts {
        if part.text().await.is_ok_and(|text| !text.is_empty()) {
            periods += 1;
        }
    }
    assert!(periods >= 1);
    let mut minute = PERIOD_MINUTES * (periods - 1);

    let event_time_element = row.find(Locator::Css(".eventTime")).await;
    if let Ok(event_time_element) = event_time_element {
        minute += event_time_element
            .text()
            .await
            .map_or(0, |text| text.parse().unwrap());
        Ok(GameTime::Playing(minute))
    } else {
        // It must be break otherwise
        minute += PERIOD_MINUTES;
        Ok(GameTime::BreakAfter(minute))
    }
}

async fn get_score(client: &mut Client) -> anyhow::Result<GameResult> {
    let base_url = Url::parse(TEAM_URL)?;
    client.goto(base_url.as_str()).await?;

    // wait for a reasonable time before we inspect DOM
    tokio::time::sleep(Duration::from_millis(200)).await;

    let match_rows = client.find_all(Locator::Css(".event__match")).await?;

    let last_match_row = &match_rows
        .first()
        .ok_or(anyhow::anyhow!("could not find .event__match element"))?;

    let home_team = last_match_row
        .find(Locator::Css(".event__participant--home"))
        .await?
        .text()
        .await?;

    let away_team = last_match_row
        .find(Locator::Css(".event__participant--away"))
        .await?
        .text()
        .await?;

    let home_score = last_match_row
        .find(Locator::Css(".event__score--home"))
        .await?
        .text()
        .await?
        .parse::<u32>()?;

    let away_score = last_match_row
        .find(Locator::Css(".event__score--away"))
        .await?
        .text()
        .await?
        .parse::<u32>()?;

    let is_live = last_match_row
        .attr("class")
        .await
        .is_ok_and(|attr| attr.is_some_and(|cname| cname.contains("event__match--live")));
    let game_time = if is_live {
        get_minute_of_game(last_match_row).await?
    } else {
        GameTime::Played
    };

    client.goto("about:blank").await?;

    let now = Local::now();

    let latest_match = if home_team.starts_with(TEAM_NAME) {
        GameResult {
            my_team: home_team,
            my_team_score: home_score,
            opponent_team: away_team,
            opponent_team_score: away_score,
            generated: now,
            game_time,
        }
    } else {
        GameResult {
            my_team: away_team,
            my_team_score: away_score,
            opponent_team: home_team,
            opponent_team_score: home_score,
            generated: now,
            game_time,
        }
    };

    Ok(latest_match)
}

// let's set up the sequence of steps we want the browser to take
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let mut driver = start_driver()?;

    let cap: Capabilities =
        serde_json::from_str(r#"{"goog:chromeOptions":{"args":["--headless"]}}"#).unwrap();

    let mut c = ClientBuilder::rustls()?
        .capabilities(cap)
        .connect(&format!("http://localhost:{DRIVER_PORT}"))
        .await
        .expect("failed to connect to WebDriver");

    loop {
        match get_score(&mut c).await {
            Ok(latest_match) => {
                info!("latest match = {latest_match:?}");
                serde_json::to_writer_pretty(File::create(cli.output.clone())?, &latest_match)?;
            }
            Err(error) => {
                warn!("got error: {error}");
            }
        }

        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("exitting the main loop");
                break;
            },
            _ = tokio::time::sleep(Duration::from_secs(cli.refresh)) => {
            }
        }
    }

    driver.kill().unwrap();

    c.close().await?;

    Ok(())
}
