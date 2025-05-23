use reqwest::{Client, multipart};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use log::{info, warn, error, debug};
use crate::soundcloud::Track;

/// Response data from a Discord webhook
#[derive(Debug, Clone)]
pub struct WebhookResponse {
    pub message_id: String,
    pub channel_id: Option<String>,
}

/// Send a track to Discord via webhook
pub async fn send_track_webhook(
    webhook_url: &str, 
    track: &Track,
    audio_files: Option<Vec<(String, String)>> // Vec of (file_path, file_name)
) -> Result<WebhookResponse, Box<dyn std::error::Error + Send + Sync>> {
    // Create the webhook client
    let client = Client::new();
    
    // Add wait=true parameter to webhook URL
    let webhook_url = if webhook_url.contains('?') {
        format!("{}&wait=true", webhook_url)
    } else {
        format!("{}?wait=true", webhook_url)
    };
    
    // Build the embed object
    info!("Preparing Discord webhook for track '{}' (ID: {})", track.title, track.id);
    let embed = build_track_embed(track);
    
    // Check audio files
    let files_count = match &audio_files {
        Some(files) => files.len(),
        None => 0,
    };
    
    // If we have audio files, we need to use multipart/form-data
    // Otherwise, we can just use a simple JSON post
    let result = if let Some(files) = audio_files {
        if files.is_empty() {
            debug!("No audio files attached, sending embed only");
            send_embed_only(client, &webhook_url, embed).await
        } else {
            debug!("Attaching {} audio files to webhook", files.len());
            send_with_audio_files(client, &webhook_url, embed, files).await
        }
    } else {
        debug!("No audio files provided, sending embed only");
        send_embed_only(client, &webhook_url, embed).await
    };
    
    // Log result
    match &result {
        Ok(response) => info!("Successfully sent Discord webhook for track '{}' with {} audio files. Message ID: {}", 
                           track.title, files_count, response.message_id),
        Err(e) => error!("Failed to send Discord webhook for track '{}': {}", track.title, e),
    }
    
    result
}

/// Build a Discord embed for the track
fn build_track_embed(track: &Track) -> Value {
    debug!("Building Discord embed for track '{}' (ID: {})", track.title, track.id);
    
    // Extract additional metadata from raw_data if available
    let mut description = track.description.clone().unwrap_or_default();
    
    // Trim description to 2000 characters to avoid Discord payload size limits
    const MAX_DESCRIPTION_LENGTH: usize = 2000;
    if description.len() > MAX_DESCRIPTION_LENGTH {
        warn!("Track description for '{}' exceeded Discord limit ({} chars), trimming to {} chars",
            track.title, description.len(), MAX_DESCRIPTION_LENGTH);
        description.truncate(MAX_DESCRIPTION_LENGTH);
        // Add ellipsis to indicate truncation
        description.push_str("...");
    }
    
    // These values will be populated from either raw_data or track struct directly
    let play_count: Option<u64>;
    let likes_count: Option<u64>;
    let reposts_count: Option<u64>;
    let comment_count: Option<u64>;
    let genre: Option<String>;
    let tags: Option<String>;
    
    if let Some(raw_data) = &track.raw_data {
        // Get play count
        play_count = raw_data.get("playback_count").and_then(|v| v.as_u64());
        
        // Get likes count
        likes_count = raw_data.get("likes_count").and_then(|v| v.as_u64());
        
        // Get reposts count
        reposts_count = raw_data.get("reposts_count").and_then(|v| v.as_u64());
        
        // Get comment count
        comment_count = raw_data.get("comment_count").and_then(|v| v.as_u64());
        
        // Get genre
        genre = raw_data.get("genre").and_then(|v| v.as_str()).map(String::from);
        
        // Get tags
        tags = raw_data.get("tag_list").and_then(|v| v.as_str()).map(String::from);
        
    } else {
        // Use values from the track struct directly if available
        play_count = track.playback_count;
        likes_count = track.likes_count;
        reposts_count = track.reposts_count;
        comment_count = track.comment_count;
        genre = track.genre.clone();
        tags = track.tag_list.clone();
    }
    
    debug!("Track metadata - plays: {:?}, likes: {:?}, reposts: {:?}, comments: {:?}", 
           play_count, likes_count, reposts_count, comment_count);
    
    // Build fields for the embed
    let mut fields = vec![];
    
    // Add duration if available
    if track.duration > 0 {
        let duration_secs = track.duration / 1000;
        let minutes = duration_secs / 60;
        let seconds = duration_secs % 60;
        fields.push(json!({
            "name": "Duration",
            "value": format!("{}:{:02}", minutes, seconds),
            "inline": true
        }));
    }
    
    // Add genre if available
    if let Some(g) = genre {
        if !g.is_empty() {
            fields.push(json!({
                "name": "Genre",
                "value": g,
                "inline": true
            }));
        }
    }
    
    // Add tags as a separate field if available
    if let Some(tag_list) = tags {
        if !tag_list.is_empty() {
            let parsed_tags = parse_tags(&tag_list);
            if !parsed_tags.is_empty() {
                fields.push(json!({
                    "name": "Tags",
                    "value": parsed_tags.join(", "),
                    "inline": false
                }));
            }
        }
    }
    
    debug!("Created {} embed fields for Discord message", fields.len());
    
    // Get original high-resolution artwork URL if available
    let artwork_url = track.artwork_url.clone()
        .map(|url| crate::soundcloud::get_original_artwork_url(&url))
        .unwrap_or_default();
    
    // Create the embed object
    json!({
        "title": track.title,
        "type": "rich",
        "description": description,
        "url": track.permalink_url,
        "timestamp": track.created_at,
        "color": 0xFF7700, // SoundCloud orange
        "author": {
            "name": track.user.username.clone(),
            "url": track.user.permalink_url.clone(),
            "icon_url": track.user.avatar_url.clone().unwrap_or_default()
        },
        "thumbnail": {
            "url": artwork_url
        },
        "fields": fields,
        "footer": {
            "text": "SoundCloud Archiver • All available audio formats are attached"
        }
    })
}

/// Parse a tag list string, respecting quoted tags
/// 
/// Handles:
/// - Space-separated individual tags
/// - Tags enclosed in double quotes (treated as a single tag)
/// - Supports nested quotes
fn parse_tags(tag_list: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut current_tag = String::new();
    let mut in_quotes = false;
    let mut escape_next = false;
    
    for c in tag_list.chars() {
        match (c, in_quotes, escape_next) {
            // Handle escape character
            ('\\', _, false) => {
                escape_next = true;
            },
            // Start or end quote
            ('"', _, true) => {
                current_tag.push('"');
                escape_next = false;
            },
            ('"', false, false) => {
                in_quotes = true;
            },
            ('"', true, false) => {
                in_quotes = false;
            },
            // Space handling
            (' ', false, false) => {
                if !current_tag.is_empty() {
                    tags.push(current_tag);
                    current_tag = String::new();
                }
            },
            // Regular character
            (_, _, true) => {
                current_tag.push('\\');
                current_tag.push(c);
                escape_next = false;
            },
            (_, _, false) => {
                current_tag.push(c);
            }
        }
    }
    
    // Don't forget the last tag if there is one
    if !current_tag.is_empty() {
        tags.push(current_tag);
    }
    
    tags
}

/// Send just the embed without any files
async fn send_embed_only(
    client: Client, 
    webhook_url: &str, 
    embed: Value
) -> Result<WebhookResponse, Box<dyn std::error::Error + Send + Sync>> {
    debug!("Preparing embed-only Discord webhook request");
    
    let payload = json!({
        "embeds": [embed],
        "username": "SoundCloud Archiver",
    });
    
    debug!("Sending webhook POST request to Discord");
    let response = client
        .post(webhook_url)
        .json(&payload)
        .send()
        .await?;
    
    let status = response.status();
    debug!("Discord API response status: {}", status);
    
    if !status.is_success() {
        let error_text = response.text().await?;
        error!("Discord webhook error: {} - {}", status, error_text);
        return Err(format!("Discord webhook error: {} - {}", status, error_text).into());
    }
    
    // Parse the response JSON to get the message ID
    let response_json: Value = response.json().await?;
    let message_id = match response_json.get("id") {
        Some(id) => {
            match id.as_str() {
                Some(id_str) => id_str.to_string(),
                None => {
                    error!("Failed to extract message ID from Discord response");
                    return Err("Failed to extract message ID from Discord response".into());
                }
            }
        },
        None => {
            error!("No message ID in Discord response");
            return Err("No message ID in Discord response".into());
        }
    };
    
    // Extract channel_id if available
    let channel_id = response_json.get("channel_id")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    
    debug!("Discord webhook sent successfully, message ID: {}", message_id);
    Ok(WebhookResponse { message_id, channel_id })
}

/// Send the embed with audio file attachments
async fn send_with_audio_files(
    client: Client,
    webhook_url: &str,
    embed: Value,
    files: Vec<(String, String)> // Vec of (file_path, file_name)
) -> Result<WebhookResponse, Box<dyn std::error::Error + Send + Sync>> {
    debug!("Preparing multipart request with {} audio files", files.len());
    
    // Discord limits: 
    // - Max 8MB per file for regular uploads 
    // - Max 10 attachments per message
    const MAX_DISCORD_UPLOAD_SIZE: u64 = 8 * 1024 * 1024; // 8MB per file
    const MAX_ATTACHMENTS: usize = 8;
    
    // Filter files to respect Discord limits
    let mut filtered_files = Vec::new();
    let mut file_count = 0;
    
    // First pass: get all files and their sizes
    let mut file_sizes = Vec::new();
    for (file_path, file_name) in files {
        let path = Path::new(&file_path);
        let file_size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(e) => {
                warn!("Failed to get file size for {}: {}", file_path, e);
                0
            }
        };
        file_sizes.push((file_path, file_name, file_size));
    }
    
    // Sort files by priority as specified:
    // 1. m4a and ogg files first
    // 2. JSON metadata files
    // 3. Artwork files 
    // 4. MP3 files last
    file_sizes.sort_by(|(path_a, _, size_a), (path_b, _, size_b)| {
        // Get file extensions for easier comparison
        let ext_a = Path::new(path_a).extension().and_then(|e| e.to_str()).unwrap_or("");
        let ext_b = Path::new(path_b).extension().and_then(|e| e.to_str()).unwrap_or("");
        
        // First priority: M4A and OGG files
        let is_m4a_ogg_a = ext_a == "m4a" || ext_a == "ogg" || ext_a == "opus";
        let is_m4a_ogg_b = ext_b == "m4a" || ext_b == "ogg" || ext_b == "opus";
        if is_m4a_ogg_a && !is_m4a_ogg_b {
            return std::cmp::Ordering::Less;
        }
        if !is_m4a_ogg_a && is_m4a_ogg_b {
            return std::cmp::Ordering::Greater;
        }
        
        // Second priority: JSON metadata files
        let is_json_a = ext_a == "json";
        let is_json_b = ext_b == "json";
        if is_json_a && !is_json_b {
            return std::cmp::Ordering::Less;
        }
        if !is_json_a && is_json_b {
            return std::cmp::Ordering::Greater;
        }
        
        // Third priority: Artwork files
        let is_image_a = ext_a == "jpg" || ext_a == "jpeg" || ext_a == "png";
        let is_image_b = ext_b == "jpg" || ext_b == "jpeg" || ext_b == "png";
        if is_image_a && !is_image_b {
            return std::cmp::Ordering::Less;
        }
        if !is_image_a && is_image_b {
            return std::cmp::Ordering::Greater;
        }
        
        // MP3 files come last automatically
        // For files of the same type, prefer smaller files first
        size_a.cmp(size_b)
    });
    
    // Add files until we hit the attachment limit
    let file_sizes_len = file_sizes.len();
    for (file_path, file_name, file_size) in file_sizes {
        // Check if we would exceed limits by adding this file
        if file_count >= MAX_ATTACHMENTS {
            warn!("Reached Discord attachment limit of {} files", MAX_ATTACHMENTS);
            break;
        }
        
        // Check each file individually against the 8MB limit
        if file_size > MAX_DISCORD_UPLOAD_SIZE {
            warn!("File {} exceeds Discord size limit ({} > {})", 
                 file_name, file_size, MAX_DISCORD_UPLOAD_SIZE);
            continue;
        }
        
        // Add the file
        filtered_files.push((file_path, file_name));
        file_count += 1;
    }
    
    if filtered_files.len() < file_sizes_len {
        warn!("Some files were excluded due to Discord limits: {} of {} files included",
             filtered_files.len(), file_sizes_len);
    }
    
    // Create a multipart form
    let mut form = multipart::Form::new()
        .text("payload_json", json!({
            "embeds": [embed],
            "username": "SoundCloud Archiver",
        }).to_string());
    
    // Add each audio file
    for (i, (file_path, file_name)) in filtered_files.iter().enumerate() {
        // Read the file
        debug!("Adding file {}/{} to multipart form: {}", i+1, filtered_files.len(), file_name);
        
        let path = Path::new(file_path);
        let file_size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(e) => {
                warn!("Failed to get file size for {}: {}", file_path, e);
                0
            }
        };
        
        let mut file = match File::open(path).await {
            Ok(f) => {
                debug!("Opened file: {} ({} bytes)", file_path, file_size);
                f
            },
            Err(e) => {
                error!("Failed to open file {}: {}", file_path, e);
                return Err(format!("Failed to open file {}: {}", file_path, e).into());
            }
        };
        
        let mut buffer = Vec::new();
        match file.read_to_end(&mut buffer).await {
            Ok(size) => debug!("Read {} bytes from file {}", size, file_path),
            Err(e) => {
                error!("Failed to read file {}: {}", file_path, e);
                return Err(format!("Failed to read file {}: {}", file_path, e).into());
            }
        }
        
        // Determine MIME type
        let mime_type = match path.extension() {
            Some(ext) if ext == "mp3" => "audio/mpeg",
            Some(ext) if ext == "ogg" => "audio/ogg",
            Some(ext) if ext == "opus" => "audio/opus",
            Some(ext) if ext == "m4a" => "audio/mp4",
            Some(ext) if ext == "json" => "application/json",
            Some(ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg",
            Some(ext) if ext == "png" => "image/png",
            Some(ext) => {
                let ext_str = ext.to_string_lossy();
                debug!("Unknown extension '{}', using default MIME type", ext_str);
                "application/octet-stream"
            }
            None => {
                debug!("No file extension, using default MIME type");
                "application/octet-stream"
            }
        };
        
        // Add to form
        debug!("Adding part to form: file{} as {} (MIME: {})", i, file_name, mime_type);
        let part = multipart::Part::bytes(buffer)
            .file_name(file_name.clone())
            .mime_str(mime_type)?;
        form = form.part(format!("file{}", i), part);
    }
    
    // Send the form
    debug!("Sending multipart POST request to Discord webhook");
    let response = client
        .post(webhook_url)
        .multipart(form)
        .send()
        .await?;
    
    let status = response.status();
    debug!("Discord API response status: {}", status);
    
    if !status.is_success() {
        let error_text = response.text().await?;
        error!("Discord webhook error: {} - {}", status, error_text);
        return Err(format!("Discord webhook error: {} - {}", status, error_text).into());
    }
    
    // Parse the response JSON to get the message ID
    let response_json: Value = response.json().await?;
    let message_id = match response_json.get("id") {
        Some(id) => {
            match id.as_str() {
                Some(id_str) => id_str.to_string(),
                None => {
                    error!("Failed to extract message ID from Discord response");
                    return Err("Failed to extract message ID from Discord response".into());
                }
            }
        },
        None => {
            error!("No message ID in Discord response");
            return Err("No message ID in Discord response".into());
        }
    };
    
    // Extract channel_id if available
    let channel_id = response_json.get("channel_id")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    
    debug!("Discord webhook with files sent successfully, message ID: {}", message_id);
    Ok(WebhookResponse { message_id, channel_id })
} 