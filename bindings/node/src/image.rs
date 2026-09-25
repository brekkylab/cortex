//! `Image` and `Step`: what a session's commands run on.
//!
//! Both are values, as they are in Rust — `Image` is `Clone` and a builder call there hands
//! back a new one — so here every method returns a new object and none mutates the one it
//! was called on. A base image built once and extended in two directions stays the base.

use cortex::image::{Image, Step};
use napi::bindgen_prelude::{ClassInstance, Either};
use napi_derive::napi;

use crate::error::{self, Result};

#[napi(js_name = "Step")]
#[derive(Clone)]
pub struct JsStep(Step);

#[napi]
impl JsStep {
    #[napi(factory)]
    pub fn run(cmd: String) -> Self {
        JsStep(Step::run(cmd))
    }

    #[napi(factory)]
    pub fn copy(src: String, dst: String) -> Self {
        JsStep(Step::copy(src, dst))
    }

    #[napi(factory)]
    pub fn env(key: String, value: String) -> Self {
        JsStep(Step::env(key, value))
    }

    #[napi(factory)]
    pub fn workdir(dir: String) -> Self {
        JsStep(Step::workdir(dir))
    }

    #[napi(js_name = "toString")]
    pub fn to_js_string(&self) -> String {
        self.0.to_string()
    }
}

/// A step as a caller may spell one: a `Step`, or a string meaning `Step.run(..)` — the
/// same conversion `From<&str> for Step` makes in Rust.
pub type StepLike<'env> = Either<ClassInstance<'env, JsStep>, String>;

fn step(step: StepLike) -> Step {
    match step {
        Either::A(step) => step.0.clone(),
        Either::B(cmd) => Step::run(cmd),
    }
}

#[napi(js_name = "Image")]
#[derive(Clone)]
pub struct JsImage(pub(crate) Image);

#[napi]
impl JsImage {
    #[napi(constructor)]
    pub fn new(
        base: Option<String>,
        #[napi(ts_arg_type = "Array<Step | string>")] steps: Option<Vec<StepLike>>,
    ) -> Self {
        let mut image = Image::new().steps(steps.unwrap_or_default().into_iter().map(step));
        if let Some(base) = base {
            image = image.base(base);
        }
        JsImage(image)
    }

    #[napi(factory)]
    pub fn from_dockerfile(content: String) -> Result<Self> {
        Image::from_dockerfile(content)
            .map(JsImage)
            .map_err(error::anyhow)
    }

    #[napi]
    pub fn base(&self, base: String) -> JsImage {
        JsImage(self.0.clone().base(base))
    }

    #[napi]
    pub fn step(&self, #[napi(ts_arg_type = "Step | string")] step: StepLike) -> JsImage {
        JsImage(self.0.clone().step(self::step(step)))
    }

    #[napi]
    pub fn steps(
        &self,
        #[napi(ts_arg_type = "Array<Step | string>")] steps: Vec<StepLike>,
    ) -> JsImage {
        JsImage(self.0.clone().steps(steps.into_iter().map(step)))
    }

    #[napi(js_name = "toString")]
    pub fn to_js_string(&self) -> String {
        let steps: Vec<String> = self.0.steps.iter().map(|s| s.to_string()).collect();
        format!("Image(base={:?}, steps={steps:?})", self.0.base)
    }
}
