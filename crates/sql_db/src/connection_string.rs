//! ADO.NET-style SQL Server connection strings, as SSMS, Azure Data Studio and
//! the Fabric portal produce them.

use crate::FabricConfig;

const DEFAULT_PORT: u16 = 1433;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionStringError {
    #[error("the connection string has no Server")]
    MissingServer,
    #[error("the connection string has no Database or Initial Catalog")]
    MissingDatabase,
    #[error("invalid port in Server: {0}")]
    InvalidPort(String),
    #[error("unterminated quote in the value of {0}")]
    UnterminatedQuote(String),
    #[error(
        "Authentication={0} is not supported yet; use an Entra ID method such as ActiveDirectoryInteractive or ActiveDirectoryDefault"
    )]
    UnsupportedAuthentication(String),
    #[error("SQL logins (User ID and Password) are not supported yet; use Entra ID authentication")]
    SqlLoginNotSupported,
}

/// Entra methods that can use the signed-in Azure CLI session's token.
const ENTRA_METHODS: [&str; 5] = [
    "activedirectorydefault",
    "activedirectoryinteractive",
    "activedirectoryintegrated",
    "activedirectorydevicecodeflow",
    "activedirectorymanagedidentity",
];

/// Parses a connection string into a Fabric/SQL Server target.
///
/// A string with no `=` is taken as a bare server name, which is what the Fabric
/// portal's "SQL connection string" copy button gives; the database then comes
/// from elsewhere, so it may be empty.
pub fn parse_connection_string(text: &str) -> Result<FabricConfig, ConnectionStringError> {
    let text = text.trim();
    if !text.contains('=') {
        let (host, port) = parse_server(text)?;
        return Ok(FabricConfig {
            port,
            ..FabricConfig::new(host, "")
        });
    }

    let mut server = None;
    let mut database = None;
    let mut authentication = None;
    let mut has_user = false;
    for (key, value) in pairs(text)? {
        match normalize_key(&key).as_str() {
            "server" | "datasource" | "address" | "addr" | "networkaddress" => {
                server = Some(value);
            }
            "database" | "initialcatalog" => database = Some(value),
            "authentication" => authentication = Some(value),
            "userid" | "uid" | "user" => has_user = !value.is_empty(),
            _ => {}
        }
    }

    match authentication {
        Some(method) => {
            if !ENTRA_METHODS.contains(&normalize_key(&method).as_str()) {
                return Err(ConnectionStringError::UnsupportedAuthentication(method));
            }
        }
        None if has_user => return Err(ConnectionStringError::SqlLoginNotSupported),
        None => {}
    }

    let server = server
        .filter(|server| !server.is_empty())
        .ok_or(ConnectionStringError::MissingServer)?;
    let (host, port) = parse_server(&server)?;
    let database = database.unwrap_or_default();
    Ok(FabricConfig {
        port,
        ..FabricConfig::new(host, database)
    })
}

/// Requires a database, for callers with no other place to get one.
pub fn parse_connection_string_with_database(
    text: &str,
) -> Result<FabricConfig, ConnectionStringError> {
    let config = parse_connection_string(text)?;
    if config.database.is_empty() {
        return Err(ConnectionStringError::MissingDatabase);
    }
    Ok(config)
}

/// Keys ignore case and spaces: `Initial Catalog` and `initialcatalog` are the same.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// `tcp:host,port`, `host,port`, or `host`.
fn parse_server(server: &str) -> Result<(String, u16), ConnectionStringError> {
    let server = server.trim();
    let server = server
        .get(..4)
        .filter(|prefix| prefix.eq_ignore_ascii_case("tcp:"))
        .map_or(server, |_| &server[4..]);
    if server.is_empty() {
        return Err(ConnectionStringError::MissingServer);
    }
    match server.split_once(',') {
        Some((host, port)) => {
            let port = port
                .trim()
                .parse()
                .map_err(|_| ConnectionStringError::InvalidPort(port.trim().to_owned()))?;
            Ok((host.trim().to_owned(), port))
        }
        None => Ok((server.to_owned(), DEFAULT_PORT)),
    }
}

/// Splits `key=value;` pairs. Values may be quoted with `"`, `'` or `{}`;
/// a doubled closing quote inside is an escaped quote.
fn pairs(text: &str) -> Result<Vec<(String, String)>, ConnectionStringError> {
    let mut pairs = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches([';', ' ', '\t', '\r', '\n']);
        if rest.is_empty() {
            return Ok(pairs);
        }
        let Some((key, after_key)) = rest.split_once('=') else {
            // A trailing fragment with no `=` carries no setting.
            return Ok(pairs);
        };
        let key = key.trim().to_owned();
        let after_key = after_key.trim_start();
        let (value, remaining) = match after_key.chars().next() {
            Some(open @ ('"' | '\'' | '{')) => {
                let close = if open == '{' { '}' } else { open };
                read_quoted(&after_key[1..], close)
                    .ok_or_else(|| ConnectionStringError::UnterminatedQuote(key.clone()))?
            }
            _ => match after_key.split_once(';') {
                Some((value, remaining)) => (value.trim().to_owned(), remaining),
                None => (after_key.trim().to_owned(), ""),
            },
        };
        pairs.push((key, value));
        rest = remaining;
    }
}

fn read_quoted(text: &str, close: char) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if character != close {
            value.push(character);
            continue;
        }
        if characters.peek().is_some_and(|&(_, next)| next == close) {
            value.push(close);
            characters.next();
            continue;
        }
        let remaining = &text[index + close.len_utf8()..];
        return Some((value, remaining));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_azure_data_studio_style_strings() {
        let config = parse_connection_string(
            "Server=tcp:abc.datawarehouse.fabric.microsoft.com,1433;Initial Catalog=analytics;\
             Encrypt=True;TrustServerCertificate=False;Connection Timeout=30;\
             Authentication=Active Directory Interactive;",
        )
        .unwrap();
        assert_eq!(config.host, "abc.datawarehouse.fabric.microsoft.com");
        assert_eq!(config.port, 1433);
        assert_eq!(config.database, "analytics");
    }

    #[test]
    fn accepts_synonyms_case_and_quotes() {
        let config = parse_connection_string(
            "data source = host.example,14330 ; DATABASE='my;db' ; authentication = \"ActiveDirectoryDefault\"",
        )
        .unwrap();
        assert_eq!(config.host, "host.example");
        assert_eq!(config.port, 14330);
        assert_eq!(config.database, "my;db");
        let config = parse_connection_string("Server=h;Database={a}}b}").unwrap();
        assert_eq!(config.database, "a}b");
    }

    #[test]
    fn bare_server_name_is_allowed() {
        let config = parse_connection_string("abc.datawarehouse.fabric.microsoft.com").unwrap();
        assert_eq!(config.host, "abc.datawarehouse.fabric.microsoft.com");
        assert_eq!(config.port, 1433);
        assert_eq!(config.database, "");
        assert_eq!(
            parse_connection_string_with_database("abc.datawarehouse.fabric.microsoft.com"),
            Err(ConnectionStringError::MissingDatabase)
        );
    }

    #[test]
    fn rejects_logins_other_than_entra() {
        assert_eq!(
            parse_connection_string("Server=h;Database=d;User ID=sa;Password=x"),
            Err(ConnectionStringError::SqlLoginNotSupported)
        );
        assert_eq!(
            parse_connection_string("Server=h;Authentication=SqlPassword"),
            Err(ConnectionStringError::UnsupportedAuthentication(
                "SqlPassword".to_owned()
            ))
        );
    }

    #[test]
    fn reports_malformed_strings() {
        assert_eq!(
            parse_connection_string("Database=d"),
            Err(ConnectionStringError::MissingServer)
        );
        assert_eq!(
            parse_connection_string("Server=h,abc"),
            Err(ConnectionStringError::InvalidPort("abc".to_owned()))
        );
        assert_eq!(
            parse_connection_string("Server=h;Database='open"),
            Err(ConnectionStringError::UnterminatedQuote(
                "Database".to_owned()
            ))
        );
    }
}
