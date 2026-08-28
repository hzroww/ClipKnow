//! 两个进程同时读写同一个库会怎样 —— 验证 WAL + busy_timeout 那个改动。
//!
//! 跑法（db 要先 init 过，避免迁移的写操作混进竞争）：
//!   probe_wal init   <db>          建表
//!   probe_wal writer <db>          占着写锁 3 秒
//!   probe_wal reader <db>          在那 3 秒里读
//!   probe_wal writer2 <db>         在那 3 秒里也想写
//! LEGACY=1 时降级回加 WAL 之前的行为，用来对照。
use clipknow::store::sqlite::SqliteStore;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (role, db) = (a[1].as_str(), a[2].as_str());
    // ★ 计时从 open **之前**开始。open() 要跑迁移，那也是写操作——
    //   等待大部分发生在它里面。第一版把 Instant::now() 放在 open 之后，
    //   于是竞争测出来是 0.00 秒，完全骗人。
    let t = std::time::Instant::now();
    let st = match SqliteStore::open(db) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "{role:<8} ★open 就失败★ {e}（等了 {:.2} 秒）",
                t.elapsed().as_secs_f64()
            );
            return;
        }
    };
    if std::env::var("LEGACY").is_ok() {
        st.downgrade_to_legacy_for_test().expect("downgrade");
    }

    match role {
        "init" => println!("init     journal_mode = {}", st.journal_mode_for_test()),
        "writer" => match st.hold_write_lock_for_test(3) {
            Ok(()) => println!("writer   写锁持有 3 秒，已释放"),
            Err(e) => println!("writer   ★失败★ {e}"),
        },
        "reader" => match st.count_sessions_for_test() {
            Ok(n) => println!(
                "reader   读到 {n} 行，等了 {:.2} 秒",
                t.elapsed().as_secs_f64()
            ),
            Err(e) => println!(
                "reader   ★失败★ {e}（等了 {:.2} 秒）",
                t.elapsed().as_secs_f64()
            ),
        },
        "writer2" => match st.hold_write_lock_for_test(0) {
            Ok(()) => println!(
                "writer2  也写成功了，等了 {:.2} 秒",
                t.elapsed().as_secs_f64()
            ),
            Err(e) => println!(
                "writer2  ★失败★ {e}（等了 {:.2} 秒）",
                t.elapsed().as_secs_f64()
            ),
        },
        _ => unreachable!(),
    }
}
