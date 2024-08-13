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
use tokio::time::sleep;
use tracing::{debug, info, warn};
use url::Url;

const DRIVER_PORT: u16 = 9515;

#[derive(Debug, Serialize)]
enum GameTime {
    WillBePlayed,
    Played,
    BreakAfter(u64),
    Playing(u64),
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct GameResult {
    my_team: String,
    my_team_score: u64,
    opponent_team: String,
    opponent_team_score: u64,
    game_time: GameTime,
    generated: DateTime<Local>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Livescore URL of the team
    url: Url,

    /// Team name
    team_name: String,

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

const PERIOD_MINUTES: u64 = 20;

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
            .map_or(0, |text| text.parse().unwrap_or_default());
        Ok(GameTime::Playing(minute))
    } else {
        // It must be break otherwise
        minute += PERIOD_MINUTES;
        Ok(GameTime::BreakAfter(minute))
    }
}

async fn get_latest_match_element(client: &mut Client) -> anyhow::Result<Option<Element>> {
    for _ in 0..10 {
        sleep(Duration::from_millis(200)).await;
        let last_match_row = client
            .find_all(Locator::Css(".event__match"))
            .await?
            .into_iter()
            .next();
        if last_match_row.is_some() {
            return Ok(last_match_row);
        }
        debug!("sleeping in find_all for .event__match");
    }

    Ok(None)
}

async fn get_score(client: &mut Client, url: &Url, team_name: &str) -> anyhow::Result<GameResult> {
    client.goto(url.as_str()).await?;

    // wait for a reasonable time before we inspect DOM
    tokio::time::sleep(Duration::from_millis(500)).await;

    let last_match_row = get_latest_match_element(client)
        .await?
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
        .parse()
        .unwrap_or_default();

    let away_score = last_match_row
        .find(Locator::Css(".event__score--away"))
        .await?
        .text()
        .await?
        .parse()
        .unwrap_or_default();

    let last_match_class = last_match_row
        .attr("class")
        .await?
        .ok_or(anyhow::anyhow!("class attribute should not be empty"))?;

    let game_time = if last_match_class.contains("event__match--live") {
        get_minute_of_game(&last_match_row).await?
    } else if last_match_class.contains("event__match--scheduled") {
        GameTime::WillBePlayed
    } else {
        GameTime::Played
    };

    client.goto("about:blank").await?;

    let now = Local::now();

    let latest_match = if home_team.starts_with(team_name) {
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
        match get_score(&mut c, &cli.url, &cli.team_name).await {
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
