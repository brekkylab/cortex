#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

pub trait Executable: Send + Sync {
    fn exec(&self, program: String, args: Vec<String>) -> ExecResult;
}
