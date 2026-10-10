/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: a small ad and tracker blocker. A request to a listed site (or any of its
//! sub-domains) that is not the page itself is cancelled before it is sent. The list is short
//! on purpose: only well-known advertising and tracking networks that pages do not need to work.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(true);
static BLOCKED: AtomicU64 = AtomicU64::new(0);

const LIST: &[&str] = &[
    // Google advertising and analytics
    "doubleclick.net", "googlesyndication.com", "googleadservices.com", "adservice.google.com",
    "2mdn.net", "google-analytics.com", "analytics.google.com", "googletagservices.com",
    "adsense.google.com", "partner.googleadservices.com",
    // Ad exchanges and networks
    "adnxs.com", "adsrvr.org", "taboola.com", "outbrain.com", "criteo.com", "criteo.net",
    "rubiconproject.com", "pubmatic.com", "openx.net", "casalemedia.com", "indexww.com",
    "amazon-adsystem.com", "advertising.com", "adform.net", "smartadserver.com", "teads.tv",
    "yieldmo.com", "bidswitch.net", "sharethrough.com", "33across.com", "adcolony.com",
    "zedo.com", "serving-sys.com", "mathtag.com", "lijit.com", "sovrn.com", "contextweb.com",
    "gumgum.com", "triplelift.com", "sonobi.com", "adroll.com", "onetag-sys.com", "media.net",
    "revcontent.com", "mgid.com", "pubmine.com", "adtechus.com", "adtech.de", "yieldlab.net",
    "improvedigital.com", "richaudience.com", "adition.com", "admixer.net", "adskeeper.com",
    "undertone.com", "spotxchange.com", "spotx.tv", "springserve.com", "tremorhub.com",
    "freewheel.tv", "innovid.com", "moatads.com", "adsafeprotected.com", "doubleverify.com",
    "flashtalking.com", "everesttech.net", "adgrx.com", "cpmstar.com", "adblade.com",
    // Pop-ups and aggressive ad networks
    "popads.net", "popcash.net", "propellerads.com", "exoclick.com", "juicyads.com",
    "trafficjunky.net", "adsterra.com", "clickadu.com", "hilltopads.net", "trafficfactory.biz",
    // Tracking and data brokers
    "scorecardresearch.com", "quantserve.com", "quantcount.com", "bluekai.com", "krxd.net",
    "demdex.net", "tapad.com", "agkn.com", "rlcdn.com", "liadm.com", "chartbeat.com",
    "chartbeat.net", "hotjar.com", "crazyegg.com", "mouseflow.com", "fullstory.com",
    "omtrdc.net", "2o7.net", "adobedtm.com", "bounceexchange.com", "mookie1.com",
    "exelator.com", "eyeota.net", "turn.com", "BrightTag.com", "semasio.net", "id5-sync.com",
    // Social network trackers
    "ads-twitter.com", "ads.linkedin.com", "px.ads.linkedin.com", "analytics.tiktok.com",
    "ads.tiktok.com", "business-api.tiktok.com", "pixel.facebook.com", "ads.pinterest.com",
    "ct.pinterest.com", "tr.snapchat.com", "sc-static.net", "bat.bing.com", "clarity.ms",
    // Others
    "mc.yandex.ru", "an.yandex.ru", "yandexadexchange.net", "ad.mail.ru", "top-fwz1.mail.ru",
    "adriver.ru", "umeng.com", "hm.baidu.com",
];

fn set() -> &'static HashSet<String> {
    static SET: OnceLock<HashSet<String>> = OnceLock::new();
    SET.get_or_init(|| LIST.iter().map(|d| d.to_ascii_lowercase()).collect())
}

pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn blocked_count() -> u64 {
    BLOCKED.load(Ordering::Relaxed)
}

pub(crate) fn note_blocked() {
    BLOCKED.fetch_add(1, Ordering::Relaxed);
}

/// Is `host` a listed site or a sub-domain of one?
pub(crate) fn host_is_listed(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let set = set();
    let mut rest = host.as_str();
    loop {
        if set.contains(rest) {
            return true;
        }
        match rest.split_once('.') {
            Some((_, tail)) if tail.contains('.') || set.contains(tail) => rest = tail,
            _ => return false,
        }
    }
}

/// Should this request be cancelled? Pages themselves are never blocked.
pub(crate) fn should_block(url: &url::Url, is_main_frame: bool) -> bool {
    if is_main_frame || !ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    url.host_str().is_some_and(host_is_listed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(url: &str) -> bool {
        should_block(&url::Url::parse(url).unwrap(), false)
    }

    #[test]
    fn blocks_listed_sites_and_their_subdomains() {
        assert!(blocked("https://doubleclick.net/x.js"));
        assert!(blocked("https://securepubads.g.doubleclick.net/tag/js/gpt.js"));
        assert!(blocked("http://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js"));
        assert!(blocked("https://ADS.Example.criteo.com/a"));
    }

    #[test]
    fn leaves_ordinary_sites_alone() {
        assert!(!blocked("https://www.wikipedia.org/"));
        assert!(!blocked("https://fonts.gstatic.com/s/x.woff2"));
        assert!(!blocked("https://notdoubleclick.net/x"));
        assert!(!blocked("https://doubleclick.net.evil.example/x"));
        assert!(!blocked("https://net/x"));
        assert!(!blocked("http://127.0.0.1:8080/s/abc"));
        assert!(!blocked("file:///C:/x.js"));
    }

    #[test]
    fn never_blocks_the_page_itself_and_can_be_turned_off() {
        let url = url::Url::parse("https://doubleclick.net/").unwrap();
        assert!(!should_block(&url, true));
        set_enabled(false);
        assert!(!blocked("https://doubleclick.net/x.js"));
        set_enabled(true);
        assert!(blocked("https://doubleclick.net/x.js"));
    }

    #[test]
    fn list_is_clean() {
        for domain in LIST {
            assert!(domain.contains('.') && !domain.contains(' ') && !domain.contains('/'), "{domain}");
        }
    }
}
