use std::sync::Arc;

use handlebars::Handlebars;

use log::{debug, error};

use serde::{Deserialize, Serialize};

use warp::http::Response;
use warp::{Filter, Rejection, Reply};

use crate::git::GitError;
use crate::warp_helper::{ContentType, ResponseBuilder};
use crate::{GitRepository, Path, SmeagolError};

const INDEX_FILE: &'static str = "index.md";

pub struct Smeagol {
    handlebars: Arc<Handlebars>,
}
impl Smeagol {
    pub fn new() -> Result<Smeagol, SmeagolError> {
        debug!("Initializing");
        Ok(Smeagol {
            handlebars: Arc::new(Self::initialize_handlebars()?),
        })
    }
    fn initialize_handlebars() -> Result<Handlebars, SmeagolError> {
        debug!("Initializing Handlebars");
        let mut handlebars = Handlebars::new();
        handlebars.register_templates_directory(".hbs", "templates/")?;

        Ok(handlebars)
    }

    pub fn start(self) {
        debug!("Starting on 127.0.0.1:8000");

        warp::serve(self.routes()).run(([127, 0, 0, 1], 8000));
    }

    fn routes(&self) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
        self.index()
            .or(self.edit())
            .or(self.get())
            .with(warp::log::log("smeagol"))
            .recover(Self::recover_500())
    }
    fn recover_500(
    ) -> impl Fn(warp::Rejection) -> Result<warp::http::Response<String>, Rejection> + Clone {
        |err: warp::Rejection| {
            if let Some(ref err) = err.find_cause::<SmeagolError>() {
                error!("Internal error: {}", err);
                Ok(ResponseBuilder::new()
                    .header(warp::http::header::CONTENT_TYPE, ContentType::Plain)
                    .status(500)
                    .body("An internal error occurred.".to_string()))
            } else {
                Err(err)
            }
        }
    }

    fn index(&self) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
        warp::path::end().map(|| "Hello!")
    }

    fn get(&self) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
        #[derive(Serialize)]
        struct TemplateGetData {
            path: String,
            content: String,
        }
        #[derive(Serialize)]
        struct TemplateGetNotFoundData {
            path: String,
        }
        warp::get2()
            .and(
                warp::path::full()
                    .map(|fullpath: warp::filters::path::FullPath| fullpath.as_str().to_string()),
            )
            .and(self.templates())
            .and_then(
                |path: String, templates: Arc<Handlebars>| -> Result<Response<String>, Rejection> {
                    let path = Path::from_percent_encoded(path.as_bytes());

                    let repo = GitRepository::new("repo")?;
                    let item = repo.item(path.clone())?;

                    match item.content() {
                        Ok(content) => Ok(ResponseBuilder::new()
                            .header(warp::http::header::CONTENT_TYPE, ContentType::Html)
                            .status(200)
                            .body_template(
                                &templates,
                                "get.html",
                                &TemplateGetData {
                                    path: path.to_string(),
                                    // TODO handle non-utf content
                                    content: String::from_utf8_lossy(&content[..]).to_string(),
                                },
                            )?),
                        Err(GitError::IsDir) => {
                            let mut redirect_path = path;
                            redirect_path.push(INDEX_FILE.to_string());
                            Ok(ResponseBuilder::new()
                                .status(302)
                                .header(
                                    warp::http::header::LOCATION,
                                    format!("/{}", redirect_path.percent_encode()),
                                )
                                .body("".to_string()))
                        }
                        // TODO allow editing if the file could exist
                        Err(GitError::NotFound) => Ok(ResponseBuilder::new()
                            .header(warp::http::header::CONTENT_TYPE, ContentType::Html)
                            .status(404)
                            .body_template(
                                &templates,
                                "get_not_found.html",
                                &TemplateGetNotFoundData {
                                    path: path.to_string(),
                                },
                            )?),
                        Err(err) => Err(err.into()),
                    }
                },
            )
    }

    fn edit(&self) -> impl Filter<Extract = impl Reply, Error = warp::Rejection> + Clone {
        #[derive(Deserialize)]
        struct QueryParameters {
            // This field is never accessed but is required for the tag
            #[allow(dead_code)]
            edit: String,
        }
        #[derive(Serialize)]
        struct TemplateEditCouldNotExistData {
            path: String,
        }
        #[derive(Serialize)]
        struct TemplateEditData {
            path: String,
            content: Option<String>,
        }
        warp::get2()
            .and(
                warp::path::full()
                    .map(|fullpath: warp::filters::path::FullPath| fullpath.as_str().to_string()),
            )
            .and(warp::query::<QueryParameters>())
            .and(self.templates())
            .and_then(
                |path: String,
                 _: QueryParameters,
                 templates: Arc<Handlebars>|
                 -> Result<Response<String>, Rejection> {
                    let path = Path::from_percent_encoded(path.as_bytes());

                    let repo = GitRepository::new("repo")?;
                    let item = repo.item(path.clone())?;

                    if !item.could_exist()? {
                        return Ok(ResponseBuilder::new()
                            .header(warp::http::header::CONTENT_TYPE, ContentType::Html)
                            .status(404)
                            .body_template(
                                &templates,
                                "edit_could_not_exist.html",
                                &TemplateEditCouldNotExistData {
                                    path: path.to_string(),
                                },
                            )?);
                    }

                    match item.content() {
                        Ok(content) => Ok(ResponseBuilder::new()
                            .header(warp::http::header::CONTENT_TYPE, ContentType::Html)
                            .status(200)
                            .body_template(
                                &templates,
                                "edit.html",
                                &TemplateEditData {
                                    path: path.to_string(),
                                    // TODO handle non-utf content
                                    content: Some(
                                        String::from_utf8_lossy(&content[..]).to_string(),
                                    ),
                                },
                            )?),
                        Err(GitError::NotFound) => Ok(ResponseBuilder::new()
                            .header(warp::http::header::CONTENT_TYPE, ContentType::Html)
                            .status(200)
                            .body_template(
                                &templates,
                                "edit.html",
                                &TemplateEditData {
                                    path: path.to_string(),
                                    content: None,
                                },
                            )?),
                        Err(err) => Err(err.into()),
                    }
                },
            )
    }

    fn templates(&self) -> impl Filter<Extract = (Arc<Handlebars>,), Error = Rejection> + Clone {
        let handlebars = self.handlebars.clone();
        warp::any()
            .and_then(move || -> Result<Arc<Handlebars>, Rejection> { Ok(handlebars.clone()) })
    }
}
