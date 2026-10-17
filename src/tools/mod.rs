//! MCP tool modules. Each submodule owns one `#[tool_router]` impl block on
//! [`crate::server::AppServer`]; `AppServer::tool_router` merges them.

pub mod auth;
pub mod chat;
pub mod comments;
pub mod content;
pub mod database;
pub mod docedit;
pub mod export;
pub mod imports;
pub mod meeting;
pub mod member;
pub mod notifications;
pub mod pages;
pub mod publish;
pub mod quicknotes;
pub mod search;
pub mod sharing;
pub mod upload;
pub mod util;
pub mod workspace;
