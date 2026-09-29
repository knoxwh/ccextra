pub const DEFAULT_BASE_URL: &str = "https://api2.cursor.sh";
pub const DEFAULT_AUTH_DIR: &str = ".cache/cursor";
pub const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
pub const POLL_PATH: &str = "/auth/poll";
pub const REFRESH_PATH: &str = "/auth/exchange_user_api_key";
pub const REFRESH_SKEW_SECS: i64 = 600;
pub const POLL_MAX_ATTEMPTS: usize = 150;
pub const POLL_MAX_ERRORS: usize = 10;
