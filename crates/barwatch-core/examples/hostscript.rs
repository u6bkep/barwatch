//! Prints the hosting script for a replay: `hostscript <replay.sdfz> <port> <autohost-port>`.
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 4, "usage: hostscript <replay.sdfz> <port> <autohost-port>");
    let path = std::path::Path::new(&args[1]);
    let demo = barwatch_core::demo::Demo::read(path)?;
    let script =
        barwatch_core::script::dedicated_script(&demo.script, &demo.game_data, path, args[2].parse()?, args[3].parse()?)?;
    print!("{script}");
    eprintln!("{:?} | {:?} | {:?} | {}s", demo.engine_version, demo.game_version(), demo.map(), demo.game_time_s);
    eprintln!("{:?}", demo.roster());
    Ok(())
}
