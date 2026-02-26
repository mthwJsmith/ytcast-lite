//! Test nsig decoding via yt-dlp/ejs solver in QuickJS.
//! Usage: cargo run --example test_nsig [VIDEO_ID]

fn main() {
    let video_id = std::env::args().nth(1).unwrap_or_else(|| "o8Vmp6iPx48".to_string());
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
              (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

    // 1. Load cookies
    let cookie_path = dirs::home_dir()
        .map(|h| h.join("ytm-cookies.txt"))
        .expect("no home dir");
    println!("Loading cookies from {:?}", cookie_path);
    let cookie_content = std::fs::read_to_string(&cookie_path).expect("can't read cookies");
    let mut cookie_parts = Vec::new();
    let mut sapisid: Option<String> = None;
    let mut sapisid_1p: Option<String> = None;
    let mut sapisid_3p: Option<String> = None;

    for line in cookie_content.lines() {
        let line = line.trim();
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') { continue; }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 { continue; }
        let (name, value) = (parts[5], parts[6]);
        cookie_parts.push(format!("{}={}", name, value));
        match name {
            "SAPISID" => { if sapisid.is_none() { sapisid = Some(value.split('/').next().unwrap_or(value).to_string()); } }
            "__Secure-1PAPISID" => { sapisid_1p = Some(value.split('/').next().unwrap_or(value).to_string()); }
            "__Secure-3PAPISID" => {
                let v = value.split('/').next().unwrap_or(value).to_string();
                sapisid_3p = Some(v.clone());
                if sapisid.is_none() { sapisid = Some(v); }
            }
            _ => {}
        }
    }
    let cookie_hdr = cookie_parts.join("; ");
    let sapisid = sapisid.expect("no SAPISID in cookies");
    println!("Cookies: {} entries, SAPISID=true", cookie_parts.len());

    // 2. Fetch music.youtube.com
    println!("\nFetching music.youtube.com...");
    let client = reqwest::blocking::Client::builder()
        .user_agent(ua)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    let page = client.get("https://music.youtube.com/")
        .header("Cookie", &cookie_hdr)
        .send().unwrap()
        .text().unwrap();
    println!("Page: {}B", page.len());

    // 3. Extract INNERTUBE_CONTEXT
    let ctx = extract_json_field(&page, "INNERTUBE_CONTEXT").expect("no INNERTUBE_CONTEXT");
    let client_version = ctx.get("client")
        .and_then(|c| c.get("clientVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!("Client version: {}", client_version);

    // 4. Extract player hash + fetch base.js
    let player_hash = page.split("/s/player/")
        .skip(1)
        .find_map(|part| {
            let h: String = part.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if h.len() >= 6 { Some(h) } else { None }
        })
        .expect("no player hash");

    let base_js_url = format!("https://www.youtube.com/s/player/{}/player_ias.vflset/en_US/base.js", player_hash);
    println!("\nFetching base.js (hash={})...", player_hash);
    let base_js = client.get(&base_js_url).send().unwrap().text().unwrap();
    println!("base.js: {}B", base_js.len());

    // 5. Extract STS
    let sts = base_js.split("signatureTimestamp")
        .skip(1)
        .find_map(|part| {
            let t = part.trim_start();
            if let Some(rest) = t.strip_prefix(':') {
                let num: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                if num.len() == 5 { num.parse::<u64>().ok() } else { None }
            } else { None }
        })
        .expect("no STS");
    println!("STS: {}", sts);

    // 6. Session metadata + auth
    let datasync_id = extract_string_field(&page, "DATASYNC_ID");
    let session_index = extract_string_field(&page, "SESSION_INDEX");
    let logged_in = page.contains("\"LOGGED_IN\":true") || page.contains("\"LOGGED_IN\": true");
    let usid = datasync_id.as_ref().map(|d| d.split("||").next().unwrap_or(d).to_string());
    println!("DSID: {:?}, session_idx: {:?}, logged_in: {}", usid, session_index, logged_in);

    let origin = "https://music.youtube.com";
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let compute_hash = |scheme: &str, sid: &str| -> String {
        use sha1::{Digest, Sha1};
        let (input, suffix) = if let Some(ref uid) = usid {
            (format!("{} {} {} {}", uid, ts, sid, origin), "_u")
        } else {
            (format!("{} {} {}", ts, sid, origin), "")
        };
        let hash = Sha1::digest(input.as_bytes());
        format!("{} {}_{:x}{}", scheme, ts, hash, suffix)
    };
    let mut auth_parts = vec![compute_hash("SAPISIDHASH", &sapisid)];
    if let Some(ref s) = sapisid_1p { auth_parts.push(compute_hash("SAPISID1PHASH", s)); }
    if let Some(ref s) = sapisid_3p { auth_parts.push(compute_hash("SAPISID3PHASH", s)); }
    let auth = auth_parts.join(" ");

    // 7. WEB_REMIX /player
    let body = serde_json::json!({
        "videoId": video_id,
        "context": ctx,
        "contentCheckOk": true,
        "racyCheckOk": true,
        "playbackContext": {
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
                "signatureTimestamp": sts,
            }
        }
    });

    println!("\nCalling WEB_REMIX /player for {}...", video_id);
    let resp = client.post("https://music.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("Content-Type", "application/json")
        .header("Origin", origin)
        .header("Referer", "https://music.youtube.com/")
        .header("Cookie", &cookie_hdr)
        .header("Authorization", &auth)
        .header("X-Origin", origin)
        .header("X-YouTube-Client-Name", "67")
        .header("X-YouTube-Client-Version", client_version)
        .header("X-Youtube-Bootstrap-Logged-In", if logged_in { "true" } else { "false" })
        .json(&body)
        .send().unwrap();

    let result: serde_json::Value = resp.json().unwrap();
    let status = result["playabilityStatus"]["status"].as_str().unwrap_or("?");
    let sabr_url = result["streamingData"]["serverAbrStreamingUrl"].as_str().unwrap_or("");
    println!("Status: {}", status);

    if sabr_url.is_empty() {
        println!("NO SABR URL!");
        std::process::exit(1);
    }

    // 8. Extract n param
    let n_value = sabr_url.split('?').nth(1)
        .and_then(|q| q.split('&').find(|p| p.starts_with("n=")))
        .map(|p| p[2..].to_string());
    println!("\nn param: {:?}", n_value);

    if let Some(ref n_val) = n_value {
        // 9. Decode nsig with yt-dlp/ejs solver
        println!("\nDecoding nsig with yt-dlp/ejs solver (meriyah + QuickJS)...");
        let start = std::time::Instant::now();

        match ytcast_lite::nsig::decode_nsig(&base_js, n_val, &player_hash) {
            Ok(decoded) => {
                let elapsed = start.elapsed();
                println!("Decode time: {:.2}s", elapsed.as_secs_f64());
                println!("DECODED: {} -> {}", n_val, decoded);

                // Replace n in URL and test CDN
                let new_url = if let Some(pos) = sabr_url.find("&n=").or_else(|| sabr_url.find("?n=")) {
                    let prefix_end = pos + 3;
                    let rest = &sabr_url[prefix_end..];
                    let n_end = rest.find('&').unwrap_or(rest.len());
                    format!("{}{}{}", &sabr_url[..prefix_end], decoded, &rest[n_end..])
                } else {
                    sabr_url.to_string()
                };

                println!("\n--- Testing SABR CDN with DECODED n ---");
                let test_url = format!("{}&rn=0", new_url);
                match client.post(&test_url)
                    .header("Content-Type", "application/x-protobuf")
                    .header("Accept", "application/vnd.yt-ump")
                    .body(vec![0u8])
                    .send() {
                    Ok(resp) => println!("HTTP {} ({} bytes)", resp.status(), resp.content_length().unwrap_or(0)),
                    Err(e) => println!("Error: {}", e),
                }
            }
            Err(e) => {
                let elapsed = start.elapsed();
                println!("FAILED after {:.2}s: {}", elapsed.as_secs_f64(), e);
            }
        }

        // Test second decode (should use cached preprocessed → fast)
        println!("\n--- Second decode (memory cached) ---");
        let start2 = std::time::Instant::now();
        match ytcast_lite::nsig::decode_nsig(&base_js, n_val, &player_hash) {
            Ok(decoded2) => println!("Decode2 time: {:.2}s, result: {}", start2.elapsed().as_secs_f64(), decoded2),
            Err(e) => println!("Decode2 FAILED: {}", e),
        }

        // Test with original n (should 403)
        println!("\n--- Testing SABR CDN with ORIGINAL n ---");
        let test_url = format!("{}&rn=0", sabr_url);
        match client.post(&test_url)
            .header("Content-Type", "application/x-protobuf")
            .header("Accept", "application/vnd.yt-ump")
            .body(vec![0u8])
            .send() {
            Ok(resp) => println!("HTTP {} ({} bytes)", resp.status(), resp.content_length().unwrap_or(0)),
            Err(e) => println!("Error: {}", e),
        }
    } else {
        println!("No n param — CDN should work directly");
    }

    println!("\nDone.");
}

fn extract_json_field(page: &str, field: &str) -> Option<serde_json::Value> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split("ytcfg.set(") {
        if !chunk.contains(&pattern) { continue; }
        if let Some(field_pos) = chunk.find(&pattern) {
            let after_key = &chunk[field_pos + pattern.len()..];
            let after_colon = after_key.trim_start().strip_prefix(':')?;
            let after_colon = after_colon.trim_start();
            if after_colon.starts_with('{') {
                if let Some(json_str) = find_matching_brace(after_colon) {
                    if let Ok(val) = serde_json::from_str(json_str) {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

fn extract_string_field(page: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split(&pattern) {
        let rest = chunk.trim_start();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_string());
                }
            }
        }
    }
    None
}

fn find_matching_brace(s: &str) -> Option<&str> {
    if !s.starts_with('{') { return None; }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape { escape = false; continue; }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 { return Some(&s[..=i]); }
            }
            _ => {}
        }
    }
    None
}
