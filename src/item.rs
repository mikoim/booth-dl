//! Sales page data: `item.json`, `item.html`, `images/` and `description.md`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::client::BoothClient;
use crate::library::LibraryItem;

/// The subset of `booth.pm/ja/items/<id>.json` used for `description.md`.
#[derive(Debug, Deserialize)]
struct ItemJson {
    name: String,
    description: Option<String>,
    url: Option<String>,
    price: Option<String>,
    published_at: Option<String>,
    category: Option<Named>,
    #[serde(default)]
    tags: Vec<Named>,
    #[serde(default)]
    images: Vec<Image>,
    shop: Option<Shop>,
    order: Option<Order>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Image {
    original: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Shop {
    name: String,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Order {
    purchased_at: Option<String>,
}

/// Saves the sales page of `item` into `dir`. Falls back to library data
/// when the page no longer exists.
pub fn save_item_page(client: &BoothClient, item: &LibraryItem, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let page_url = format!("{}/ja/items/{}", client.booth_url(), item.item_id);

    let markdown = match client.get_text_optional(&format!("{page_url}.json"))? {
        Some(text) => {
            fs::write(dir.join("item.json"), &text)?;
            let json: ItemJson = serde_json::from_str(&text).context("parse item.json")?;
            save_images(client, &json.images, &dir.join("images"))?;
            render_description(&json)
        }
        None => render_from_library(item, &page_url),
    };
    if let Some(html) = client.get_text_optional(&page_url)? {
        fs::write(dir.join("item.html"), html)?;
    }
    fs::write(dir.join("description.md"), markdown)?;
    Ok(())
}

fn save_images(client: &BoothClient, images: &[Image], dir: &Path) -> Result<()> {
    let urls: Vec<&str> = images
        .iter()
        .filter_map(|i| i.original.as_deref())
        .collect();
    if urls.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for (index, url) in urls.into_iter().enumerate() {
        let bytes = client.get_bytes(url)?;
        let name = format!("{:02}.{}", index + 1, image_extension(url));
        fs::write(dir.join(name), bytes)?;
    }
    Ok(())
}

/// Extension of the last URL path segment, or `jpg` if it has none.
fn image_extension(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let file = path.rsplit('/').next().unwrap_or_default();
    match file.rsplit_once('.') {
        Some((_, ext))
            if !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_lowercase()
        }
        _ => "jpg".to_string(),
    }
}

fn render_description(json: &ItemJson) -> String {
    let mut fields = Vec::new();
    if let Some(shop) = &json.shop {
        fields.push(match &shop.url {
            Some(url) => format!("Shop: [{}]({url})", shop.name),
            None => format!("Shop: {}", shop.name),
        });
    }
    if let Some(url) = &json.url {
        fields.push(format!("URL: {url}"));
    }
    if let Some(price) = &json.price {
        fields.push(format!("Price: {price}"));
    }
    if let Some(category) = &json.category {
        fields.push(format!("Category: {}", category.name));
    }
    if !json.tags.is_empty() {
        let tags: Vec<&str> = json.tags.iter().map(|t| t.name.as_str()).collect();
        fields.push(format!("Tags: {}", tags.join(", ")));
    }
    if let Some(published) = &json.published_at {
        fields.push(format!("Published: {published}"));
    }
    if let Some(purchased) = json.order.as_ref().and_then(|o| o.purchased_at.as_ref()) {
        fields.push(format!("Purchased: {purchased}"));
    }

    let mut out = format!("# {}\n\n", json.name);
    for field in fields {
        out.push_str(&format!("- {field}\n"));
    }
    if let Some(description) = &json.description {
        out.push_str(&format!("\n## Description\n\n{}\n", description.trim_end()));
    }
    out
}

fn render_from_library(item: &LibraryItem, page_url: &str) -> String {
    format!(
        "# {}\n\n- Shop: {}\n- URL: {page_url}\n\nThe sales page is no longer available; this file was generated from the library.\n",
        item.title, item.shop_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Reply, TestServer};

    const ITEM_JSON: &str = include_str!("../tests/fixtures/item.json");

    fn library_item() -> LibraryItem {
        LibraryItem {
            item_id: 1001,
            title: "Sample Item One".into(),
            shop_name: "Sample Shop".into(),
            files: vec![],
        }
    }

    #[test]
    fn renders_description_markdown() {
        let json: ItemJson = serde_json::from_str(ITEM_JSON).unwrap();
        assert_eq!(
            render_description(&json),
            "# Sample Item One\n\n\
             - Shop: [Sample Shop](https://sample-shop.booth.pm/)\n\
             - URL: https://sample-shop.booth.pm/items/1001\n\
             - Price: ¥ 500\n\
             - Category: Illustration book\n\
             - Tags: tag-a, tag-b\n\
             - Published: 2026-01-27T11:09:42.000+09:00\n\
             - Purchased: 2026年10月24日\n\
             \n## Description\n\nLine one.\nLine two.\n"
        );
    }

    #[test]
    fn picks_image_extension() {
        assert_eq!(
            image_extension("https://x/i/1/a_base_resized.PNG?x=1"),
            "png"
        );
        assert_eq!(image_extension("https://x/i/1/noext"), "jpg");
    }

    #[test]
    fn saves_json_html_images_and_markdown() {
        let server = TestServer::start(|request| {
            let host = &request.headers["host"];
            match request.path.as_str() {
                "/ja/items/1001.json" => Reply::new(200).body(format!(
                    r#"{{"name": "N", "images": [{{"original": "http://{host}/img/a.png"}}, {{"original": "http://{host}/img/b.jpg"}}]}}"#
                )),
                "/ja/items/1001" => Reply::new(200).body("<html>page</html>"),
                "/img/a.png" => Reply::new(200).body("png-bytes"),
                "/img/b.jpg" => Reply::new(200).body("jpg-bytes"),
                _ => Reply::new(404),
            }
        });
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();

        save_item_page(&client, &library_item(), dir.path()).unwrap();

        assert!(
            fs::read_to_string(dir.path().join("item.json"))
                .unwrap()
                .contains("\"N\"")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("item.html")).unwrap(),
            "<html>page</html>"
        );
        assert_eq!(
            fs::read(dir.path().join("images/01.png")).unwrap(),
            b"png-bytes"
        );
        assert_eq!(
            fs::read(dir.path().join("images/02.jpg")).unwrap(),
            b"jpg-bytes"
        );
        assert!(
            fs::read_to_string(dir.path().join("description.md"))
                .unwrap()
                .starts_with("# N\n")
        );
        let image_request = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/img/a.png")
            .unwrap();
        assert!(!image_request.headers.contains_key("cookie"));
    }

    #[test]
    fn falls_back_to_library_data_when_the_page_is_gone() {
        let server = TestServer::start(|_| Reply::new(404));
        let client = BoothClient::for_test("s", &server.url);
        let dir = tempfile::tempdir().unwrap();

        save_item_page(&client, &library_item(), dir.path()).unwrap();

        let markdown = fs::read_to_string(dir.path().join("description.md")).unwrap();
        assert!(markdown.starts_with("# Sample Item One\n\n- Shop: Sample Shop\n"));
        assert!(markdown.contains("no longer available"));
        assert!(!dir.path().join("item.json").exists());
        assert!(!dir.path().join("item.html").exists());
    }
}
