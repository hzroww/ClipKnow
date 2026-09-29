//! Go → Rust 的身份凭证。
//!
//! ## 这一层在解决什么
//!
//! Go 那边管账号：注册、密码校验（Argon2id）、登录态 Cookie（`ck_session`）。
//! Rust 这边管会话和执行，**完全不碰浏览器 Cookie**。中间需要一个东西让
//! Rust 相信「这个请求确实代表 users 表里的某一行」。
//!
//! 做法是 Go 每转发一次请求就现签一个短命令牌，Rust 验签名、验有效期，
//! 验过之后得到一个 [`Principal`]（里面只有 `user_id`）。
//!
//! ## 三条不能破的规矩
//!
//! 1. **user_id 只能来自凭证的 `sub`，不能来自请求体或 URL。**
//!    这是设计文档第 12 节的第一条不变量。放松一次，「拿别人的 user_id
//!    去查别人的会话」就成立了。
//!
//! 2. **Go 丢弃浏览器传来的 `Authorization` 头，自己重新签。**
//!    不然浏览器可以自带一个凭证绕过 Go 的 Cookie 校验。这条规矩在 Go
//!    那一侧执行（web/agentclient.go），这里写下来是为了两边能对上。
//!
//! 3. **密钥必须配。** 没配就拒绝启动，不存在「没配就不校验」的降级路径。
//!    那种默认是最典型的上线事故——开发时一路顺畅，上线忘了配，于是
//!    一个谁都能调的服务挂在那里，而且不报任何错。
//!
//! ## 为什么用 JWT 而不是自己拼一个签名串
//!
//! 自己拼大概 50 行，能省掉 jsonwebtoken 那一串依赖。不这么做的原因是
//! **有效期、签发者、接收方这三项校验的边界情况比看上去多**（时钟漂移的
//! 容差、`exp` 缺失时算过期还是算永久、`aud` 是字符串还是数组），
//! 而这三项写错了都不会让测试变红，只会在某天让一个过期凭证被放行。
//!
//! 代价是一串用不到的依赖（RSA / ECDSA / ed25519）——jsonwebtoken 的
//! `rust_crypto` feature 是全算法一起给的，没法只要 HS256。

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::error::{ClipKnowError, Result};
use crate::serve::{ApiError, AppState};

/// 密钥从这个环境变量读。Go 和 Rust 必须配成同一个值。
pub const SECRET_ENV: &str = "CLIPKNOW_INTERNAL_SECRET";

/// 谁签的。Go 那边的 web 服务。
pub const ISSUER: &str = "clipknow-web";

/// 给谁的。Rust 这边的 agent 服务。
///
/// 签发者和接收方都要校验，是为了让这个密钥**只能**用来签这一种凭证。
/// 哪天同一个密钥被拿去签别的东西（比如给前端的什么令牌），那些凭证
/// 因为 `aud` 对不上会在这里被拒，不会自动变成一张万能通行证。
pub const AUDIENCE: &str = "clipknow-agent";

/// 密钥最短长度。
///
/// HS256 的安全性全压在密钥的熵上。抓到一个凭证之后可以离线暴力破解密钥，
/// 短密钥几秒就出来了——而拿到密钥就等于能冒充任意用户。
/// 32 字节是随手生成一个就能满足的量（`openssl rand -base64 32`），
/// 挡的是「图省事写了个 dev」这种情况。
const MIN_SECRET_LEN: usize = 32;

/// 凭证里的内容。
///
/// 字段名是 JWT 的标准短名，Go 那边签的时候必须一字不差地对上。
/// `web/internaltoken_test.go` 里有一条固定的黄金样例把两边钉在一起。
#[derive(Debug, Deserialize)]
struct Claims {
    /// subject —— users.id。**唯一的身份来源。**
    sub: String,
    // iss / aud / exp 由 jsonwebtoken 的 Validation 校验，
    // 不需要在这里声明成字段。
}

/// 验过之后的可信身份。
///
/// 里面**只有** user_id：别的东西（用户名、是不是管理员、登录态 id）
/// 放进来就会诱使下游拿它做判断，而那些信息在 Go 那一侧才是权威的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
}

/// 校验器。启动时建一次，之后所有请求共用。
pub struct Verifier {
    key: DecodingKey,
    validation: Validation,
}

/// **手写而不是 derive**：`DecodingKey` 里装着密钥。derive 出来的 Debug
/// 会在任何一句 `dbg!`、`{:?}` 或 panic 消息里把它打出去，而日志是会被
/// 收集、转发、翻阅的。密钥泄漏一次就等于任何人都能冒充任意用户。
impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verifier{ 密钥不打印 }")
    }
}

impl Verifier {
    /// 从环境变量读密钥。没配或太短都直接报错。
    pub fn from_env() -> Result<Self> {
        let secret = crate::env_var(SECRET_ENV).ok_or(ClipKnowError::MissingEnv(SECRET_ENV))?;
        if secret.len() < MIN_SECRET_LEN {
            return Err(ClipKnowError::BadRequest(format!(
                "{SECRET_ENV} 太短（{} 字符，至少要 {MIN_SECRET_LEN}）。\n\
                 生成一个：openssl rand -base64 32\n\
                 Go 和 Rust 两边必须配成同一个值。",
                secret.len()
            )));
        }
        Ok(Self::new(secret.as_bytes()))
    }

    /// 直接给密钥。测试用，以及 `from_env` 自己调。
    pub fn new(secret: &[u8]) -> Self {
        // ★ 算法写死成 HS256。
        //
        //   `Validation::new(alg)` 的含义是「只接受这一种算法」——凭证 header
        //   里写别的（比如 `"alg":"none"`，或者把 HMAC 换成 RSA 公钥当密钥用
        //   的那个经典绕过）一律拒。**绝不能**用 header 里的 alg 去挑验证方式，
        //   那正是 JWT 最有名的那类漏洞。
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        // exp 必须有。默认就是要求的，显式写出来是因为「忘了写 exp 的凭证
        // 等于永久有效」这个后果太重，值得在代码里留个痕迹。
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        // 时钟容差。两个进程在同一台机器上，本来不需要；留 5 秒是为了
        // 容器里偶发的时钟跳变，不至于让一个刚签出来的凭证被判成「还没生效」。
        validation.leeway = 5;

        Verifier {
            key: DecodingKey::from_secret(secret),
            validation,
        }
    }

    /// 验一个凭证，成功返回身份。
    ///
    /// 失败时的 `message` 带具体原因（过期 / 签名不对 / aud 不对）。
    /// **刻意不做成一句笼统的「凭证无效」**：唯一的调用方是我们自己的 Go
    /// 服务，而配错密钥、两边时钟差太多这类问题，靠一句笼统的话要查很久。
    /// 泄漏面也确实是零——猜不出密钥的人拿到「过期了」这三个字没有任何用。
    pub fn verify(&self, token: &str) -> std::result::Result<Principal, ApiError> {
        match jsonwebtoken::decode::<Claims>(token, &self.key, &self.validation) {
            Ok(data) => {
                // sub 是空串时 Validation 不管——它只检查字段在不在。
                // 空的 user_id 会让下游的归属查询匹配到 0 行（表现成「没有会话」），
                // 那是个很难查的静默失败，在这里拦住。
                if data.claims.sub.trim().is_empty() {
                    return Err(unauthorized("凭证里的 sub 是空的"));
                }
                Ok(Principal {
                    user_id: data.claims.sub,
                })
            }
            Err(e) => Err(unauthorized(format!("凭证无效: {e}"))),
        }
    }
}

fn unauthorized(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

/// 让 `Principal` 能直接写进 handler 的参数表。
///
/// 写成 axum 的提取器（而不是一个「每个 handler 记得调一次」的函数）是
/// 刻意的：**忘了调**是这类代码最常见的漏洞，而忘了写参数不会漏——
/// 没有 `Principal` 参数的 handler 根本拿不到 user_id，也就没法查任何
/// 属于用户的数据。漏洞从「运行时静默越权」变成「编译期写不出来」。
impl FromRequestParts<Arc<AppState>> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .ok_or_else(|| unauthorized("缺少 Authorization 头"))?
            .to_str()
            .map_err(|_| unauthorized("Authorization 头不是合法的 ASCII"))?;

        // 只认 "Bearer <token>"。大小写不敏感是 RFC 6750 要求的。
        let token = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))
            .ok_or_else(|| unauthorized("Authorization 头不是 Bearer 形式"))?;

        state.verifier.verify(token.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    const SECRET: &[u8] = b"test-secret-at-least-32-bytes-long!!";

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// 按给定的 claims 签一个凭证。测试里用它造各种坏凭证。
    fn sign(secret: &[u8], claims: serde_json::Value) -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    fn good_claims() -> serde_json::Value {
        json!({
            "sub": "u_abc",
            "iss": ISSUER,
            "aud": AUDIENCE,
            "iat": now(),
            "exp": now() + 120,
        })
    }

    #[test]
    fn 正常的凭证能验过并拿到_user_id() {
        let v = Verifier::new(SECRET);
        let p = v.verify(&sign(SECRET, good_claims())).unwrap();
        assert_eq!(p.user_id, "u_abc");
    }

    #[test]
    fn 过期的凭证被拒() {
        let v = Verifier::new(SECRET);
        let mut c = good_claims();
        // 减 3600 而不是减 1：Validation 有 5 秒容差
        c["exp"] = json!(now() - 3600);
        c["iat"] = json!(now() - 7200);
        let e = v.verify(&sign(SECRET, c)).unwrap_err();
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn 换一个密钥签的凭证被拒() {
        let v = Verifier::new(SECRET);
        let other = b"another-secret-also-32-bytes-long!!!";
        let e = v.verify(&sign(other, good_claims())).unwrap_err();
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn 接收方不对的凭证被拒() {
        let v = Verifier::new(SECRET);
        let mut c = good_claims();
        c["aud"] = json!("someone-else");
        assert!(v.verify(&sign(SECRET, c)).is_err());
    }

    #[test]
    fn 签发者不对的凭证被拒() {
        let v = Verifier::new(SECRET);
        let mut c = good_claims();
        c["iss"] = json!("not-clipknow");
        assert!(v.verify(&sign(SECRET, c)).is_err());
    }

    #[test]
    fn 没有_exp_的凭证被拒() {
        let v = Verifier::new(SECRET);
        let mut c = good_claims();
        c.as_object_mut().unwrap().remove("exp");
        // 没有 exp = 永久有效。必须拒，不能当成「没设过期时间」放行。
        assert!(v.verify(&sign(SECRET, c)).is_err());
    }

    #[test]
    fn sub_是空串的凭证被拒() {
        let v = Verifier::new(SECRET);
        let mut c = good_claims();
        c["sub"] = json!("   ");
        let e = v.verify(&sign(SECRET, c)).unwrap_err();
        assert!(e.message.contains("sub"), "{}", e.message);
    }

    /// ★ 经典的 JWT 绕过：把 header 里的 alg 改成 "none"，签名部分留空。
    ///
    /// 库如果按 header 里的 alg 去挑验证方式，这个凭证就会被当成「不需要
    /// 签名」而放行——等于任何人都能自己造一个凭证冒充任意用户。
    /// `Validation::new(Algorithm::HS256)` 的意思是**只接受 HS256**，
    /// 所以这里必须拒。这条测试钉住这件事。
    #[test]
    fn alg_改成_none_的凭证被拒() {
        let v = Verifier::new(SECRET);
        let b64 = |s: &str| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
        };
        let header = b64(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64(&good_claims().to_string());
        // 第三段（签名）留空，正是 alg=none 的形状
        let forged = format!("{header}.{payload}.");
        assert!(
            v.verify(&forged).is_err(),
            "alg=none 的伪造凭证被放行了，这是能冒充任意用户的漏洞"
        );
    }

    /// 同样的绕过换个算法名：HS256 换成 HS384，密钥不变。
    ///
    /// 这个能签出合法签名，只是算法不对。固定算法之后必须拒。
    #[test]
    fn 换一种_hmac_算法的凭证也被拒() {
        let v = Verifier::new(SECRET);
        let t = jsonwebtoken::encode(
            &Header::new(Algorithm::HS384),
            &good_claims(),
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap();
        assert!(v.verify(&t).is_err());
    }

    #[test]
    fn 密钥太短时_from_env_直接报错() {
        // SAFETY: 变量名是这条测试独占的
        unsafe { std::env::set_var(SECRET_ENV, "short") };
        let e = Verifier::from_env().unwrap_err();
        unsafe { std::env::remove_var(SECRET_ENV) };
        let msg = e.to_string();
        assert!(msg.contains("太短"), "{msg}");
        // 报错里要带上补救办法，不然看的人还得去翻文档
        assert!(msg.contains("openssl rand"), "{msg}");
    }

    /// ★ 跨语言的黄金样例。
    ///
    /// 下面这个凭证是 **Go 那边** `web/internaltoken.go` 的 `signFor` 真实
    /// 产出的（固定密钥、固定时间戳，所以结果是确定的）。
    /// `web/internaltoken_test.go` 里有一条测试断言 Go 能原样产出它。
    ///
    /// 两条测试合起来钉住的是：**Go 签的东西 Rust 认得**。
    /// 少了这个，Go 那边改个字段名（比如把 `sub` 写成 `user_id`）在两边
    /// 各自的单元测试里都是绿的，要等端到端测试才炸——而端到端跑得慢、
    /// 报错离现场远。
    #[test]
    fn go_签出来的凭证_rust_认得() {
        const GOLDEN_SECRET: &[u8] = b"golden-secret-for-cross-language-test";
        const GOLDEN_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJhdWQiOiJjbGlwa25vdy1hZ2VudCIsImV4cCI6MTcwMDAwMDEyMCwiaWF0IjoxNzAwMDAwMDAwLCJpc3MiOiJjbGlwa25vdy13ZWIiLCJzdWIiOiJ1X2dvbGRlbiJ9.OjKLCydFro1aP4_fcxNqRdivjYdWQu36Af7XxBaeB8M";

        // 黄金凭证里的 exp 是 2023 年的固定时间戳，必然过期。校验有效期
        // 不是这条测试的目的——它验的是**字段名和签名算法两边对得上**，
        // 所以用一个不管有效期的 Validation。
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        validation.validate_exp = false;

        let data = jsonwebtoken::decode::<Claims>(
            GOLDEN_TOKEN,
            &DecodingKey::from_secret(GOLDEN_SECRET),
            &validation,
        )
        .expect("Go 签出来的凭证 Rust 验不过——两边的字段名或算法对不上了");
        assert_eq!(data.claims.sub, "u_golden");
    }
}
