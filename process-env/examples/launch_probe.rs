fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    tyde_process_env::launch::exec_helper_entry();
    tyde_process_env::init_process_env()?;
    println!("Login-shell PATH initialized successfully");
    Ok(())
}
