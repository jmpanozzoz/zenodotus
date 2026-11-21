//! robots.txt rules for one site, matched against the product token of the configured user agent.

use std::time::Duration;

use texting_robots::Robot;
use url::Url;

/// The longest `Crawl-delay` honoured; longer ones are capped so a sync cannot stall.
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(60);

/// The rules of one robots.txt file.
#[derive(Debug)]
pub struct Robots {
    robot: Option<Robot>,
}

impl Robots {
    /// No rules: everything is allowed, as for a site without robots.txt.
    #[must_use]
    pub const fn allow_all() -> Self {
        Self { robot: None }
    }

    /// The rules of `txt` for `user_agent`. A file that cannot be parsed allows everything.
    #[must_use]
    pub fn parse(user_agent: &str, txt: &[u8]) -> Self {
        Self {
            robot: Robot::new(product_token(user_agent), txt).ok(),
        }
    }

    /// Whether `url` may be fetched.
    #[must_use]
    pub fn allowed(&self, url: &Url) -> bool {
        self.robot.as_ref().is_none_or(|robot| robot.allowed(url.as_str()))
    }

    /// The `Crawl-delay`, capped at [`MAX_CRAWL_DELAY`]; none when absent, zero, negative or not a number.
    #[must_use]
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.robot
            .as_ref()
            .and_then(|robot| robot.delay)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| Duration::from_secs_f32(seconds.min(MAX_CRAWL_DELAY.as_secs_f32())))
    }

    /// The `Sitemap:` URLs, resolved against `base`, skipping invalid ones.
    #[must_use]
    pub fn sitemaps(&self, base: &Url) -> Vec<Url> {
        self.robot
            .iter()
            .flat_map(|robot| robot.sitemaps.iter())
            .filter_map(|sitemap| base.join(sitemap).ok())
            .collect()
    }
}

/// The product token of a user agent: `zenodotus` for `zenodotus/0.1.0 (+https://…)`.
fn product_token(user_agent: &str) -> &str {
    user_agent.split(['/', ' ']).next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "zenodotus/0.1.0 (+https://github.com/zenodotus-docs/zenodotus)";

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn rules_for_the_product_token_win_over_the_wildcard() {
        let robots = Robots::parse(
            AGENT,
            b"User-agent: *\nDisallow: /\n\nUser-agent: zenodotus\nDisallow: /private/\nAllow: /private/ok\nCrawl-delay: 2\n\nSitemap: https://docs.example/sitemap.xml\nSitemap: /relative.xml\n",
        );
        assert!(robots.allowed(&url("https://docs.example/guide")));
        assert!(!robots.allowed(&url("https://docs.example/private/x")));
        assert!(robots.allowed(&url("https://docs.example/private/ok")));
        assert_eq!(robots.crawl_delay(), Some(Duration::from_secs(2)));
        let sitemaps: Vec<String> = robots
            .sitemaps(&url("https://docs.example/robots.txt"))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            sitemaps,
            ["https://docs.example/sitemap.xml", "https://docs.example/relative.xml"]
        );
    }

    #[test]
    fn other_agents_follow_the_wildcard() {
        let robots = Robots::parse("other-bot/2", b"User-agent: *\nDisallow: /x\n");
        assert!(!robots.allowed(&url("https://docs.example/x/y")));
        assert!(robots.allowed(&url("https://docs.example/y")));
        assert_eq!(robots.crawl_delay(), None);
    }

    #[test]
    fn crawl_delays_are_capped_and_nonsense_is_ignored() {
        let delay = |value: &str| {
            Robots::parse(AGENT, format!("User-agent: *\nCrawl-delay: {value}\n").as_bytes()).crawl_delay()
        };
        assert_eq!(delay("86400"), Some(MAX_CRAWL_DELAY));
        assert_eq!(delay("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(delay("0"), None);
        assert_eq!(delay("-3"), None);
        assert_eq!(delay("soon"), None);
    }

    #[test]
    fn without_rules_everything_is_allowed() {
        let robots = Robots::allow_all();
        assert!(robots.allowed(&url("https://docs.example/anything")));
        assert_eq!(robots.crawl_delay(), None);
        assert_eq!(robots.sitemaps(&url("https://docs.example/")), []);
        assert_eq!(product_token(""), "");
        assert_eq!(product_token("bot"), "bot");
    }
}
