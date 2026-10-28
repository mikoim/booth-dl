//! Library pages (`accounts.booth.pm/library`): parsing and pagination.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

use crate::client::BoothClient;

/// Stops runaway pagination if the "next" link never disappears.
const MAX_PAGES: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Purchases,
    Gifts,
    FreeDownloads,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Purchases, Tab::Gifts, Tab::FreeDownloads];

    fn path(self) -> &'static str {
        match self {
            Tab::Purchases => "/library",
            Tab::Gifts => "/library/gifts",
            Tab::FreeDownloads => "/library/free_downloads",
        }
    }

    pub fn url(self, accounts_url: &str, page: u32) -> String {
        format!("{accounts_url}{}?page={page}", self.path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryItem {
    pub item_id: u64,
    pub title: String,
    pub shop_name: String,
    pub files: Vec<LibraryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFile {
    pub downloadable_id: u64,
    /// Audio format (`flac`, `mp3`, ...) of a music track; `None` for regular files.
    pub format: Option<String>,
    pub name: String,
}

impl LibraryFile {
    /// Identifies the file in `booth.pm/downloadables/<key>` and in the state:
    /// `123` for regular files, `123?type=flac` for music tracks.
    pub fn key(&self) -> String {
        match &self.format {
            Some(format) => format!("{}?type={format}", self.downloadable_id),
            None => self.downloadable_id.to_string(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LibraryPage {
    pub items: Vec<LibraryItem>,
    pub has_next: bool,
    /// Titles of cards without any download button we understand.
    pub unsupported: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Library {
    pub items: Vec<LibraryItem>,
    pub unsupported: Vec<String>,
}

/// One entry of a music track button's `data-dropdown-items`.
#[derive(Debug, Deserialize)]
struct TrackOption {
    /// `https://booth.pm/downloadables/<id>?type=<format>`
    path: String,
    data: TrackData,
}

#[derive(Debug, Deserialize)]
struct TrackData {
    filename: String,
}

struct Selectors {
    card: Selector,
    item_link: Selector,
    title: Selector,
    thumbnail: Selector,
    shop_link: Selector,
    file_name: Selector,
    download: Selector,
    track: Selector,
    any_button: Selector,
    next: Selector,
}

impl Selectors {
    fn new() -> Selectors {
        let parse = |s| Selector::parse(s).expect("valid selector");
        Selectors {
            card: parse("div.bg-white"),
            item_link: parse(r#"a[href*="/items/"]"#),
            title: parse("div.font-bold"),
            thumbnail: parse("img.l-library-item-thumbnail"),
            shop_link: parse(r#"a[href$=".booth.pm/"]"#),
            file_name: parse("div.whitespace-pre-line > div.text-14"),
            download: parse(r#"div[data-test="downloadable"]"#),
            track: parse(r#"div[data-test="track-download-button"]"#),
            any_button: parse("div.js-download-button"),
            next: parse(r#"a[rel="next"]"#),
        }
    }
}

/// Parses one library page.
pub fn parse_library_page(html: &str) -> Result<LibraryPage> {
    let selectors = Selectors::new();
    let document = Html::parse_document(html);
    let mut page = LibraryPage::default();
    for (index, card) in document.select(&selectors.card).enumerate() {
        let is_item_card = card.select(&selectors.thumbnail).next().is_some()
            || card.select(&selectors.any_button).next().is_some();
        if !is_item_card {
            continue;
        }
        let item = parse_card(&selectors, card).with_context(|| format!("library card {index}"))?;
        if item.files.is_empty() {
            page.unsupported.push(item.title);
        } else {
            page.items.push(item);
        }
    }
    page.has_next = document.select(&selectors.next).next().is_some();
    Ok(page)
}

fn parse_card(selectors: &Selectors, card: ElementRef) -> Result<LibraryItem> {
    let item_id = card
        .select(&selectors.item_link)
        .next()
        .and_then(|a| a.value().attr("href"))
        .and_then(|href| number_after(href, "/items/"))
        .or_else(|| {
            card.select(&selectors.thumbnail)
                .next()
                .and_then(|img| img.value().attr("src"))
                .and_then(|src| number_after(src, "/i/"))
        })
        .context("no item id (neither item link nor thumbnail)")?;
    let title = card
        .select(&selectors.title)
        .next()
        .map(text_of)
        .unwrap_or_default();
    let shop_name = card
        .select(&selectors.shop_link)
        .next()
        .map(text_of)
        .unwrap_or_default();
    let mut files = regular_files(selectors, card)?;
    files.extend(track_files(selectors, card)?);
    Ok(LibraryItem {
        item_id,
        title,
        shop_name,
        files,
    })
}

/// Files with a plain download button; names and buttons appear in the same order.
fn regular_files(selectors: &Selectors, card: ElementRef) -> Result<Vec<LibraryFile>> {
    let ids = card
        .select(&selectors.download)
        .map(|el| {
            el.value()
                .attr("data-href")
                .and_then(|href| number_after(href, "/downloadables/"))
                .context("download button without a downloadable id")
        })
        .collect::<Result<Vec<u64>>>()?;
    let names: Vec<String> = card.select(&selectors.file_name).map(text_of).collect();
    if names.len() != ids.len() {
        bail!(
            "{} file names but {} download buttons",
            names.len(),
            ids.len()
        );
    }
    Ok(ids
        .into_iter()
        .zip(names)
        .map(|(downloadable_id, name)| LibraryFile {
            downloadable_id,
            format: None,
            name,
        })
        .collect())
}

/// Music tracks: one button per track, one dropdown entry per audio format.
fn track_files(selectors: &Selectors, card: ElementRef) -> Result<Vec<LibraryFile>> {
    let mut files = Vec::new();
    for button in card.select(&selectors.track) {
        let json = button
            .value()
            .attr("data-dropdown-items")
            .context("track button without dropdown items")?;
        let options: Vec<TrackOption> =
            serde_json::from_str(json).context("parse track dropdown items")?;
        for option in options {
            let downloadable_id = number_after(&option.path, "/downloadables/")
                .with_context(|| format!("no downloadable id in {}", option.path))?;
            let format = query_value(&option.path, "type")
                .filter(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric()))
                .with_context(|| format!("no audio format in {}", option.path))?;
            files.push(LibraryFile {
                downloadable_id,
                format: Some(format.to_string()),
                name: option.data.filename,
            });
        }
    }
    Ok(files)
}

fn text_of(element: ElementRef) -> String {
    element.text().collect::<String>().trim().to_string()
}

/// Parses the digits right after `marker`, e.g. `number_after("/items/12?x", "/items/") == Some(12)`.
fn number_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.find(marker)? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Value of query parameter `name` in `url`.
fn query_value<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

/// Fetches every page of every tab and merges entries that share an item ID.
pub fn fetch_library(client: &BoothClient) -> Result<Library> {
    let mut library = Library::default();
    let mut index_by_id: HashMap<u64, usize> = HashMap::new();
    for tab in Tab::ALL {
        for page_number in 1..=MAX_PAGES {
            let url = tab.url(client.accounts_url(), page_number);
            eprintln!("Fetching {url}");
            let html = client.get_text(&url)?;
            let page = parse_library_page(&html).with_context(|| format!("parse {url}"))?;
            for item in page.items {
                match index_by_id.get(&item.item_id) {
                    Some(&i) => merge_files(&mut library.items[i], item.files),
                    None => {
                        index_by_id.insert(item.item_id, library.items.len());
                        library.items.push(item);
                    }
                }
            }
            library.unsupported.extend(page.unsupported);
            if !page.has_next {
                break;
            }
        }
    }
    Ok(library)
}

fn merge_files(item: &mut LibraryItem, files: Vec<LibraryFile>) {
    for file in files {
        if !item.files.iter().any(|f| f.key() == file.key()) {
            item.files.push(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Reply, TestServer};

    const PAGE: &str = include_str!("../tests/fixtures/library_page.html");
    const LAST_PAGE: &str = include_str!("../tests/fixtures/library_last_page.html");

    fn file(downloadable_id: u64, format: Option<&str>, name: &str) -> LibraryFile {
        LibraryFile {
            downloadable_id,
            format: format.map(str::to_string),
            name: name.into(),
        }
    }

    #[test]
    fn parses_regular_and_music_items() {
        let page = parse_library_page(PAGE).unwrap();
        assert!(page.has_next);
        assert!(page.unsupported.is_empty());
        assert_eq!(
            page.items,
            vec![
                LibraryItem {
                    item_id: 1001,
                    title: "Sample Item One".into(),
                    shop_name: "Sample Shop".into(),
                    files: vec![file(2001, None, "one.pdf")],
                },
                LibraryItem {
                    item_id: 1002,
                    title: "Sample Item Two".into(),
                    shop_name: "Another Shop".into(),
                    files: vec![file(2002, None, "two-a.zip"), file(2003, None, "two-b.zip")],
                },
                LibraryItem {
                    item_id: 1004,
                    title: "Sample Album".into(),
                    shop_name: "Music Shop".into(),
                    files: vec![
                        file(2005, Some("flac"), "01-track.flac"),
                        file(2005, Some("mp3"), "01-track.mp3"),
                    ],
                },
            ]
        );
    }

    #[test]
    fn file_keys() {
        assert_eq!(file(7, None, "a").key(), "7");
        assert_eq!(file(7, Some("flac"), "a").key(), "7?type=flac");
    }

    #[test]
    fn last_page_falls_back_to_thumbnail_id_and_reports_unsupported_cards() {
        let page = parse_library_page(LAST_PAGE).unwrap();
        assert!(!page.has_next);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].item_id, 1003);
        assert_eq!(page.items[0].files, vec![file(2004, None, "three.zip")]);
        assert_eq!(page.unsupported, vec!["Item Without Downloads".to_string()]);
    }

    #[test]
    fn empty_tab_has_no_items() {
        let page =
            parse_library_page("<html><body><div class=\"bg-white\">none</div></body></html>")
                .unwrap();
        assert_eq!(page, LibraryPage::default());
    }

    #[test]
    fn mismatched_names_and_buttons_fail_loudly() {
        let html = r#"<div class="bg-white">
            <a href="https://booth.pm/ja/items/5"><div class="font-bold">T</div></a>
            <div class="js-download-button" data-test="downloadable" data-href="https://booth.pm/downloadables/6"></div>
        </div>"#;
        assert!(parse_library_page(html).is_err());
    }

    #[test]
    fn url_helpers() {
        assert_eq!(
            number_after("https://booth.pm/ja/items/12?x=1", "/items/"),
            Some(12)
        );
        assert_eq!(number_after("https://booth.pm/", "/items/"), None);
        assert_eq!(
            query_value("https://booth.pm/downloadables/7?a=1&type=flac", "type"),
            Some("flac")
        );
        assert_eq!(
            query_value("https://booth.pm/downloadables/7", "type"),
            None
        );
    }

    #[test]
    fn fetches_all_tabs_and_pages_and_merges_duplicates() {
        let server = TestServer::start(|request| {
            let body = match request.path.as_str() {
                "/library?page=1" => PAGE,
                "/library?page=2" => LAST_PAGE,
                // A gift of an item that is also purchased.
                "/library/gifts?page=1" => LAST_PAGE,
                _ => "<html></html>",
            };
            Reply::new(200).body(body)
        });
        let client = BoothClient::for_test("s", &server.url);
        let library = fetch_library(&client).unwrap();
        let ids: Vec<u64> = library.items.iter().map(|i| i.item_id).collect();
        assert_eq!(ids, vec![1001, 1002, 1004, 1003]);
        assert_eq!(library.items[3].files.len(), 1);
        assert_eq!(library.unsupported.len(), 2);
        let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(
            paths,
            vec![
                "/library?page=1",
                "/library?page=2",
                "/library/gifts?page=1",
                "/library/free_downloads?page=1",
            ]
        );
    }
}
