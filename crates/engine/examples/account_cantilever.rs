//! Produce the native account acceptance fixture from FieldCraft's original cantilever recipe.
use fieldcraft_engine::Session;
use std::{error::Error, fs::OpenOptions, io::Write};

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os().nth(1).ok_or("Pass a new .fieldcraft output path")?;
    let session = Session::new_account_model(Some("cantilever"))?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&session.account_bytes()?)?;
    Ok(())
}
