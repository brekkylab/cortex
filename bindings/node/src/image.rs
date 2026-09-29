//! `Recipe`, `Step` and `ImageSource` (what a session's commands run on), and `ImageClient`,
//! which builds, lists and removes images.
//!
//! The first three are values, as in Rust (`Recipe` is `Clone` and its builder calls return a
//! new one): every method returns a new object and none mutates its receiver, so a base recipe
//! extended in two directions stays the base.
//!
//! `ImageClient` sits in an `Arc<Mutex<Option<..>>>` beside its runtime handle: each call clones
//! it into a `'static` promise future, the lock makes calls take turns on the one channel, and
//! it is dropped inside that runtime so `quit` goes out even from a garbage-collection finalizer.

use std::sync::Arc;

use cortex::{
    image::{ImageClient, ImageEntry, ImageSource, Recipe, Step},
    protocol::BuildImageResp,
};
use napi::{
    Env,
    bindgen_prelude::{ClassInstance, Either, PromiseRaw},
};
use napi_derive::napi;
use tokio::{runtime::Handle, sync::Mutex};

use crate::{
    console::promise,
    error::{self, Result},
};

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

    #[napi]
    pub fn equals(&self, #[napi(ts_arg_type = "Step")] other: &JsStep) -> bool {
        self.0 == other.0
    }

    #[napi(js_name = "toString")]
    pub fn to_js_string(&self) -> String {
        self.0.to_string()
    }
}

/// A `Step`, or a string meaning `Step.run(..)`, as `From<&str> for Step` converts in Rust.
pub type StepLike<'env> = Either<ClassInstance<'env, JsStep>, String>;

fn step(step: StepLike) -> Step {
    match step {
        Either::A(step) => step.0.clone(),
        Either::B(cmd) => Step::run(cmd),
    }
}

#[napi(js_name = "Recipe")]
#[derive(Clone)]
pub struct JsRecipe(pub(crate) Recipe);

#[napi]
impl JsRecipe {
    #[napi(constructor)]
    pub fn new(
        base: String,
        #[napi(ts_arg_type = "Array<Step | string>")] steps: Option<Vec<StepLike>>,
    ) -> Self {
        JsRecipe(Recipe::new(base).steps(steps.unwrap_or_default().into_iter().map(step)))
    }

    #[napi(factory)]
    pub fn from_dockerfile(content: String) -> Result<Self> {
        Recipe::from_dockerfile(content)
            .map(JsRecipe)
            .map_err(error::anyhow)
    }

    #[napi(getter)]
    pub fn base(&self) -> String {
        self.0.base.clone()
    }

    #[napi]
    pub fn step(&self, #[napi(ts_arg_type = "Step | string")] step: StepLike) -> JsRecipe {
        JsRecipe(self.0.clone().step(self::step(step)))
    }

    #[napi]
    pub fn steps(
        &self,
        #[napi(ts_arg_type = "Array<Step | string>")] steps: Vec<StepLike>,
    ) -> JsRecipe {
        JsRecipe(self.0.clone().steps(steps.into_iter().map(step)))
    }

    #[napi]
    pub fn equals(&self, #[napi(ts_arg_type = "Recipe")] other: &JsRecipe) -> bool {
        self.0 == other.0
    }

    #[napi(js_name = "toString")]
    pub fn to_js_string(&self) -> String {
        let steps: Vec<String> = self.0.steps.iter().map(|s| s.to_string()).collect();
        format!("Recipe(base={:?}, steps={steps:?})", self.0.base)
    }
}

#[napi(js_name = "ImageSource")]
#[derive(Clone)]
pub struct JsImageSource(ImageSource);

#[napi]
impl JsImageSource {
    #[napi(factory)]
    pub fn reference(reference: String) -> Self {
        JsImageSource(ImageSource::reference(reference))
    }

    #[napi(factory)]
    pub fn digest(digest: String) -> Self {
        JsImageSource(ImageSource::digest(digest))
    }

    #[napi(factory)]
    pub fn recipe(#[napi(ts_arg_type = "Recipe")] recipe: &JsRecipe) -> Self {
        JsImageSource(recipe.0.clone().into())
    }

    #[napi]
    pub fn equals(&self, #[napi(ts_arg_type = "ImageSource")] other: &JsImageSource) -> bool {
        self.0 == other.0
    }

    #[napi(js_name = "toString")]
    pub fn to_js_string(&self) -> String {
        match &self.0 {
            ImageSource::Recipe { recipe } => {
                format!(
                    "ImageSource.recipe({})",
                    JsRecipe(recipe.clone()).to_js_string()
                )
            }
            ImageSource::Ref { reference } => format!("ImageSource.reference({reference:?})"),
            ImageSource::Digest { digest } => format!("ImageSource.digest({digest:?})"),
        }
    }
}

/// An `ImageSource`, or a `Recipe` meaning `ImageSource.recipe(..)`, as
/// `From<Recipe> for ImageSource` converts in Rust.
pub type ImageSourceLike<'env> =
    Either<ClassInstance<'env, JsImageSource>, ClassInstance<'env, JsRecipe>>;

pub fn image_source(image: ImageSourceLike) -> ImageSource {
    match image {
        Either::A(source) => source.0.clone(),
        Either::B(recipe) => recipe.0.clone().into(),
    }
}

#[napi(object)]
pub struct BuildImageResult {
    pub reference: String,
    pub digest: String,
}

impl From<BuildImageResp> for BuildImageResult {
    fn from(resp: BuildImageResp) -> Self {
        BuildImageResult {
            reference: resp.reference,
            digest: resp.digest,
        }
    }
}

#[napi(object, js_name = "ImageEntry")]
pub struct JsImageEntry {
    pub digest: String,
    pub refs: Vec<String>,
}

impl From<ImageEntry> for JsImageEntry {
    fn from(entry: ImageEntry) -> Self {
        JsImageEntry {
            digest: entry.digest,
            refs: entry.refs,
        }
    }
}

/// The client a slot holds, or the error for one that has been closed.
fn held(slot: &mut Option<ImageClient>) -> Result<&mut ImageClient> {
    slot.as_mut().ok_or_else(|| {
        napi::Error::new(
            "CORTEX_ERROR".to_string(),
            "this image client has been closed",
        )
    })
}

#[napi(js_name = "ImageClient")]
pub struct JsImageClient {
    client: Arc<Mutex<Option<ImageClient>>>,

    /// The runtime the client was started on; its `quit` must go out from there.
    runtime: Handle,
}

impl JsImageClient {
    /// Must run inside the future that started the client, since it keeps the current runtime.
    fn new(client: ImageClient) -> Self {
        JsImageClient {
            client: Arc::new(Mutex::new(Some(client))),
            runtime: Handle::current(),
        }
    }
}

/// Only the last holder drops the client, inside its runtime so `quit` goes out.
impl Drop for JsImageClient {
    fn drop(&mut self) {
        if let Some(slot) = Arc::get_mut(&mut self.client) {
            let _entered = self.runtime.enter();
            slot.get_mut().take();
        }
    }
}

#[napi]
impl JsImageClient {
    /// Start `cortex-krun` from the stdio server directory; settles once the server answers.
    #[napi(ts_return_type = "Promise<ImageClient>")]
    pub fn try_new(env: &Env) -> napi::Result<PromiseRaw<'_, JsImageClient>> {
        promise(env, async move {
            let client = ImageClient::try_new().await.map_err(error::failure)?;
            Ok(JsImageClient::new(client))
        })
    }

    #[napi(ts_return_type = "Promise<ImageClient>")]
    pub fn try_from_cmd(
        env: &Env,
        cmd: Vec<String>,
    ) -> napi::Result<PromiseRaw<'_, JsImageClient>> {
        promise(env, async move {
            let client = ImageClient::try_from_cmd(&cmd)
                .await
                .map_err(error::failure)?;
            Ok(JsImageClient::new(client))
        })
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn version<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, String>> {
        let client = self.client.clone();
        promise(env, async move {
            let mut slot = client.lock().await;
            held(&mut slot)?.version().await.map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<BuildImageResult>")]
    pub fn build<'env>(
        &self,
        env: &'env Env,
        #[napi(ts_arg_type = "Recipe")] recipe: &JsRecipe,
        reference: Option<String>,
    ) -> napi::Result<PromiseRaw<'env, BuildImageResult>> {
        let client = self.client.clone();
        let recipe = recipe.0.clone();
        promise(env, async move {
            let mut slot = client.lock().await;
            let resp = held(&mut slot)?.build(recipe, reference.as_deref()).await;
            resp.map(BuildImageResult::from).map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<Array<ImageEntry>>")]
    pub fn list<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, Vec<JsImageEntry>>> {
        let client = self.client.clone();
        promise(env, async move {
            let mut slot = client.lock().await;
            let resp = held(&mut slot)?.list().await;
            resp.map(|images| images.into_iter().map(JsImageEntry::from).collect())
                .map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn remove<'env>(
        &self,
        env: &'env Env,
        #[napi(ts_arg_type = "ImageSource | Recipe")] image: ImageSourceLike,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let client = self.client.clone();
        let image = image_source(image);
        promise(env, async move {
            let mut slot = client.lock().await;
            held(&mut slot)?.remove(image).await.map_err(error::failure)
        })
    }

    /// End the channel now. Closing twice is the same as closing once.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn close<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, ()>> {
        let client = self.client.clone();
        promise(env, async move {
            // Dropping on the runtime is what lets `quit` go out.
            client.lock().await.take();
            Ok(())
        })
    }
}
