//! MCP tool modules. Each submodule owns one `#[tool_router]` impl block on
//! [`crate::server::AppServer`]; `AppServer::tool_router` merges them.

pub mod auth;
pub mod chat;
pub mod content;
pub mod database;
pub mod export;
pub mod member;
pub mod pages;
pub mod publish;
pub mod quicknotes;
pub mod search;
pub mod upload;
pub mod util;
pub mod workspace;
