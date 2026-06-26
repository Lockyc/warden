use crate::colour::Colour;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub colour: Colour,
    pub icon: Option<PathBuf>,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub key: String,
    pub title: String,
    pub dir: PathBuf,
    pub cmd: String,
    pub keep_alive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub profile: String,
    pub message: String,
}
