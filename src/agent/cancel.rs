//! 取消信号：用户点了「停止」。
//!
//! 只是一个开关。取消接口把它打开，**只有循环（`runner.rs`）和工具执行器
//! （`exec.rs`）会去看它**——工具本身不知道有这回事。
//!
//! ## 循环看到信号后怎么做（三条规则，全在循环里）
//!
//! ```text
//! 两步之间（准备调模型、准备执行下一个工具）  立刻停
//! 正在调模型（回答、压缩历史）                 让这次调用做完再停
//! 正在执行任何工具                             不等了，这次调用记「已取消」；
//!                                              工具在后台跑完，结果作废
//! ```
//!
//! 第二条是产品决定：模型正在说的话让它说完。说完之后如果是最终答案，
//! 这一轮照常算完成；如果是工具请求，一个都不执行。Laplace 也是这么做的
//! （`inference.rs`：已经打开的模型流必须读完）。
//!
//! 第三条来自 Codex：工具跑在单独的线程上，取消时循环直接不等它。
//! 所以以后加工具，工具里一行取消相关的代码都不用写。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 被停止的那一轮进入下次的历史时，后面跟的一句话。
///
/// 没有它的话，模型看到的是「用户问了 X，我调了两个工具，然后用户又问了 Y」，
/// 会以为 X 已经答过了。Codex 在同一位置放的是 "The user interrupted the
/// previous turn on purpose… they may have partially executed."
///
/// 用 user 角色发（和压缩摘要一样，是运行时给的材料），所以开头写明不是
/// 用户说的话。
pub const CANCELLED_TURN_NOTE: &str = "（系统提示，不是用户说的话：用户中途停止了上面这个问题，\
     它没有得到回答。停止前工具可能已经执行了一部分。）";

/// 一轮提问的取消开关。克隆出来的都指向同一个开关。
#[derive(Clone, Default, Debug)]
pub struct CancelSignal(Arc<AtomicBool>);

impl CancelSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开开关。重复调用没关系。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 克隆出来的是同一个开关() {
        let a = CancelSignal::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        a.cancel(); // 重复打开不出事
        assert!(b.is_cancelled());
    }
}
