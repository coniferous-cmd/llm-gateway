fn main() {
    let code = llm_gateway::cli::run(
        std::env::args().skip(1).collect(),
        Box::new(std::io::stdout()),
        Box::new(std::io::stderr()),
    );
    std::process::exit(code);
}
