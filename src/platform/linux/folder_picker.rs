//! Native folder picker through the xdg-desktop-portal FileChooser, so the
//! desktop's own dialog (GTK, KDE, ...) is used without extra tools.

use std::collections::HashMap;
use std::path::PathBuf;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::super::FolderPick;

pub(crate) async fn pick_folder(title: &str) -> FolderPick {
    match portal_pick_folder(title).await {
        Ok(pick) => pick,
        Err(err) => {
            tracing::debug!(err = %err, "desktop folder picker unavailable");
            FolderPick::Unavailable
        }
    }
}

async fn portal_pick_folder(title: &str) -> zbus::Result<FolderPick> {
    let connection = zbus::Connection::session().await?;
    let Some(sender) = connection.unique_name() else {
        return Ok(FolderPick::Unavailable);
    };
    // With a handle_token the portal's request path is known up front, so the
    // Response subscription exists before the dialog can answer.
    let token = format!("herdr{}", std::process::id());
    let request_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{token}",
        sender.as_str().trim_start_matches(':').replace('.', "_")
    );
    let request = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        request_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;
    let chooser = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
    )
    .await?;
    let options = HashMap::from([
        ("handle_token", Value::from(token.as_str())),
        ("directory", Value::from(true)),
        ("accept_label", Value::from("Open")),
    ]);
    let _: OwnedObjectPath = chooser.call("OpenFile", &("", title, options)).await?;
    let Some(response) = responses.next().await else {
        return Ok(FolderPick::Unavailable);
    };
    let (code, results): (u32, HashMap<String, OwnedValue>) = response.body().deserialize()?;
    if code != 0 {
        return Ok(FolderPick::Cancelled);
    }
    let path = results
        .get("uris")
        .and_then(|uris| Vec::<String>::try_from(uris.clone()).ok())
        .and_then(|uris| uris.into_iter().next())
        .and_then(|uri| file_uri_path(&uri));
    Ok(path.map_or(FolderPick::Cancelled, FolderPick::Picked))
}

/// `file:///home/me/My%20Project` -> `/home/me/My Project`.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let escaped = (encoded[index] == b'%')
            .then(|| encoded.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                bytes.push(byte);
                index += 3;
            }
            None => {
                bytes.push(encoded[index]);
                index += 1;
            }
        }
    }
    Some(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::file_uri_path;

    #[test]
    fn file_uris_decode_to_paths() {
        assert_eq!(
            file_uri_path("file:///home/me/My%20Project%2Fx"),
            Some("/home/me/My Project/x".into())
        );
        assert_eq!(file_uri_path("file:///plain"), Some("/plain".into()));
        // A stray % is kept as-is.
        assert_eq!(file_uri_path("file:///100%"), Some("/100%".into()));
        assert_eq!(file_uri_path("https://example.com"), None);
    }
}
