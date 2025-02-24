use anyhow::Context;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime};
use clap::{Parser, ValueEnum};
use fantoccini::elements::Element;
use fantoccini::Client;
use fantoccini::{wd::Capabilities, ClientBuilder, Locator};
use serde::Serialize;
use std::fs::File;
use std::path::PathBuf;
use std::process::exit;
use std::{
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use sysinfo::System;
use tokio::signal;
use tokio::time::sleep;
use tracing::{debug, error, info};
use url::Url;

const DRIVER_PORT: u16 = 9515;

#[derive(Debug, Serialize)]
enum GameTime {
    WillBePlayed(Option<(u64, u64)>),
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

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]

enum Driver {
    Chromium,
    Firefox,
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

    /// Browser driver
    driver: Driver,

    /// Refresh interval
    #[arg(short, long, default_value_t = 30)]
    refresh: u64,

    #[arg(short, long, default_value_t = false)]
    kill_previous: bool,
}

fn get_driver_cmd(driver: Driver) -> &'static str {
    match driver {
        Driver::Chromium => "chromedriver",
        Driver::Firefox => "geckodriver",
    }
}

fn kill_previous_driver(driver: Driver) {
    let drive_cmd = get_driver_cmd(driver);

    let s = System::new_all();
    for (pid, process) in s.processes() {
        if process.name() == drive_cmd {
            process.kill();
            debug!("Killing PID {}", pid);
        }
    }
}

fn start_driver(driver: Driver) -> anyhow::Result<Child> {
    let driver = Command::new(get_driver_cmd(driver))
        .arg(format!("--port={}", DRIVER_PORT))
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;

    thread::sleep(Duration::from_millis(2000));
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
        minute += event_time_element.text().await.map_or(0, |text| {
            text.strip_suffix('\'')
                .unwrap_or(&text)
                .parse()
                .unwrap_or_default()
        });
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

fn parse_datetime(value: &str) -> anyhow::Result<NaiveDateTime> {
    let parse_time = |time: &str| -> anyhow::Result<_> {
        let time_parts = time.split_once(':').context("time should have one colon")?;
        NaiveTime::from_hms_opt(
            time_parts.0.parse().context("hour cannot be parsed")?,
            time_parts.1.parse().context("minute cannot be parsed")?,
            0,
        )
        .context("cannot parse NaiveTime")
    };

    let parse_date = |date: &str| -> anyhow::Result<_> {
        let date_parts: Vec<_> = date.split('.').collect();
        let day = date_parts.first().context("date: day part missing")?;
        let month = date_parts.get(1).context("date: month part missing")?;
        NaiveDate::from_ymd_opt(
            Local::now().year(),
            month.parse().context("month cannot be parsed")?,
            day.parse().context("day cannot be parsed")?,
        )
        .context("cannot parse NaiveDate")
    };

    if let Some((date, time)) = value.split_once(' ') {
        Ok(NaiveDateTime::new(parse_date(date)?, parse_time(time)?))
    } else {
        Ok(NaiveDateTime::new(
            Local::now().date_naive(),
            parse_time(value)?,
        ))
    }
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

    let event_time_element = last_match_row.find(Locator::Css(".event__time")).await;
    let event_time = if let Ok(event_time_element) = event_time_element {
        let match_date_time = parse_datetime(&event_time_element.text().await?)?;
        let now = Local::now().naive_local();
        debug!("Match will be played: {match_date_time}");
        if match_date_time < now {
            Some((0, 0))
        } else {
            let delta = match_date_time - now;
            Some((delta.num_hours() as u64, (delta.num_minutes() as u64) % 60))
        }
    } else {
        None
    };

    let game_time = if last_match_class.contains("event__match--live") {
        get_minute_of_game(&last_match_row).await?
    } else if last_match_class.contains("event__match--scheduled") {
        GameTime::WillBePlayed(event_time)
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

    if cli.kill_previous {
        kill_previous_driver(cli.driver);
    }

    let mut driver = start_driver(cli.driver)?;

    let cap_string = match cli.driver {
        Driver::Chromium => r#"{"goog:chromeOptions":{"args":["--headless"]}}"#,
        Driver::Firefox => r#"{"moz:firefoxOptions": {"args": ["--headless"]}}"#,
    };
    let cap: Capabilities = serde_json::from_str(cap_string).unwrap();

    let mut c = ClientBuilder::rustls()?
        .capabilities(cap)
        .connect(&format!("http://localhost:{DRIVER_PORT}"))
        .await
        .expect("failed to connect to WebDriver");

    let exit_code = loop {
        match get_score(&mut c, &cli.url, &cli.team_name).await {
            Ok(latest_match) => {
                info!("latest match = {latest_match:?}");
                serde_json::to_writer_pretty(File::create(cli.output.clone())?, &latest_match)?;
            }
            Err(err) => {
                error!("got error: {err}");
                break 1;
            }
        }

        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("exitting the main loop");
                break 0;
            },
            _ = tokio::time::sleep(Duration::from_secs(cli.refresh)) => {
            }
        }
    };

    driver.kill().unwrap();

    c.close().await?;

    exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datetime() {
        let today = Local::now();

        assert_eq!(
            parse_datetime("07.09. 18:00").unwrap().to_string(),
            format!("{}-09-07 18:00:00", today.year())
        );
        assert_eq!(
            parse_datetime("18:00").unwrap().to_string(),
            format!(
                "{}-{:02}-{:02} 18:00:00",
                today.year(),
                today.month(),
                today.day()
            )
        );
    }
}
