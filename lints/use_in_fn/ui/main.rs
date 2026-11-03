use std::fs;

macro_rules! read_env {
    () => {{
        use std::env;
        env::var("HOME")
    }};
}

mod nested {
    use std::collections::HashMap;

    pub fn map() -> HashMap<u8, u8> {
        HashMap::new()
    }
}

struct Reader;

impl Reader {
    fn read(&self) -> usize {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = std::io::empty().read_to_end(&mut buf);
        buf.len()
    }
}

fn main() {
    use std::path::Path;
    let _ = fs::read(Path::new("Cargo.toml"));
    let f = || {
        use std::time::Duration;
        Duration::from_secs(1)
    };
    let _ = f();
    if true {
        use std::cmp::max;
        let _ = max(1, 2);
    }
    let _ = read_env!();
    let _ = nested::map();
    let _ = Reader.read();
}
