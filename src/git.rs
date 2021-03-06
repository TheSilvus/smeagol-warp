use std::io;
use std::io::Write;
use std::path::Path as StdPath;

use crate::Path;

use git2::{
    Commit, ErrorCode, Object, ObjectType, Oid, Repository, RepositoryInitOptions, Signature,
    TreeBuilder,
};

pub struct GitRepository {
    repo: Repository,
}
impl GitRepository {
    pub fn new<T: AsRef<StdPath>>(dir: T) -> Result<GitRepository, GitError> {
        Ok(GitRepository {
            repo: Repository::init_opts(
                dir,
                RepositoryInitOptions::new()
                    .bare(true)
                    .mkdir(true)
                    .mkpath(false),
            )?,
        })
    }

    fn head<'repo>(&'repo self) -> Result<Commit<'repo>, GitError> {
        let head_ref = match self.repo.head() {
            Ok(head_ref) => head_ref,
            Err(err) => {
                if err.code() == ErrorCode::UnbornBranch {
                    let signature = Signature::now("smeagol", "smeagol@smeagol")?;
                    let tree_oid = self.repo.treebuilder(None)?.write()?;
                    let tree = self.repo.find_tree(tree_oid)?;
                    self.repo.commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        "Root commit",
                        &tree,
                        &[],
                    )?;
                    // We just created the head, therefore we can unwrap
                    self.repo.head().unwrap()
                } else {
                    return Err(err.into());
                }
            }
        };

        // I assume the reference given by head() is valid and a commit.
        let head_oid = head_ref.target().unwrap();
        Ok(self.repo.find_commit(head_oid).unwrap())
    }

    pub fn item<'repo>(&'repo self, path: Path) -> Result<GitItem<'repo>, GitError> {
        Ok(GitItem {
            repo: self,
            path: path,
        })
    }
}

pub struct GitItem<'repo> {
    repo: &'repo GitRepository,
    path: Path,
}
impl<'repo> GitItem<'repo> {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn parent(&self) -> Result<GitItem<'repo>, GitError> {
        if let Some(parent) = self.path.parent() {
            Ok(GitItem {
                repo: self.repo,
                path: parent,
            })
        } else {
            Err(GitError::NoParent)
        }
    }

    fn object(&self) -> Result<Object<'repo>, GitError> {
        // object could be cached but I am not sure how cache invalidation could be implemented.

        if self.path.is_empty() {
            return Ok(self.repo.head()?.tree()?.into_object());
        }

        let parent_object = self.parent()?.object()?;
        let tree = if let Ok(tree) = parent_object.into_tree() {
            tree
        } else {
            // This could also return GitError::CannotExist or something similar
            return Err(GitError::NotFound);
        };

        let potential_entry = tree
            .iter()
            // filename cannot be empty because there is a parent.
            .filter(|entry| entry.name_bytes() == self.path.filename().unwrap().bytes())
            .next();
        if let Some(entry) = potential_entry {
            Ok(entry.to_object(&self.repo.repo)?)
        } else {
            Err(GitError::NotFound)
        }
    }

    pub fn content(&self) -> Result<Vec<u8>, GitError> {
        if let Ok(blob) = self.object()?.into_blob() {
            Ok(blob.content().to_vec())
        } else {
            Err(GitError::IsDir)
        }
    }

    pub fn list(&self) -> Result<Vec<GitItem>, GitError> {
        if let Ok(tree) = self.object()?.into_tree() {
            let mut items = vec![];
            for entry in tree.iter() {
                let mut path = self.path.clone();
                path.push(entry.name_bytes().to_vec());
                items.push(self.repo.item(path)?);
            }

            Ok(items)
        } else {
            Err(GitError::IsFile)
        }
    }

    pub fn exists(&self) -> Result<bool, GitError> {
        match self.object() {
            Ok(_) => Ok(true),
            Err(GitError::NotFound) => Ok(false),
            Err(err) => Err(err),
        }
    }
    pub fn can_exist(&self) -> Result<bool, GitError> {
        if self.path.segments().count() <= 1 {
            Ok(true)
        } else {
            let parent = self.parent()?;
            if parent.exists()? && parent.is_file()? {
                Ok(false)
            } else {
                Ok(parent.can_exist()?)
            }
        }
    }

    pub fn is_dir(&self) -> Result<bool, GitError> {
        Ok(self.object()?.kind() == Some(ObjectType::Tree))
    }
    pub fn is_file(&self) -> Result<bool, GitError> {
        Ok(self.object()?.kind() == Some(ObjectType::Blob))
    }

    pub fn is_root(&self) -> bool {
        self.path.bytes().len() == 0
    }

    pub fn edit(&self, content: &[u8], message: &str) -> Result<(), GitError> {
        // I create quite a few objects that are discarded in case of an error during committing.
        // This could partially be prevented by walking the tree first and checking if the file
        // could exist because this is the most probable source of errors. I cannot delete the
        // objects manually because git2 does not support this. They can be deleted by manually
        // running `git gc` though.

        let mut blob_writer = self.repo.repo.blob_writer(None)?;
        blob_writer.write(content)?;
        let blob_oid = blob_writer.commit()?;

        let head = self.repo.head()?;
        let head_tree = head.tree()?;
        let mut tree_builder = self.repo.repo.treebuilder(Some(&head_tree))?;

        self.add_to_tree(&mut tree_builder, self.path.clone(), blob_oid)?;

        let tree_oid = tree_builder.write()?;
        let new_tree = self.repo.repo.find_tree(tree_oid)?;

        let signature = Signature::now("smeagol", "smeagol@smeagol")?;

        self.repo.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &new_tree,
            &[&head],
        )?;

        Ok(())
    }

    fn add_to_tree(
        &self,
        tree: &mut TreeBuilder,
        mut path: Path,
        object: Oid,
    ) -> Result<(), GitError> {
        assert!(!path.is_empty());

        if path.segments().count() == 1 {
            // TODO filename() essentially returns the path itself
            let filename = path.filename().unwrap();
            // The filemode of the original file is used if it already exists.
            let filemode = if let Some(entry) = tree.get(filename.bytes())? {
                if entry.kind() != Some(ObjectType::Blob) {
                    return Err(GitError::IsDir);
                }

                // The object id is essentially a hash of the object's content. We can therefore
                // compare the hashes to check if the file has changed.
                //
                // Unwrapping the object should never fail - I think
                if entry.to_object(&self.repo.repo).unwrap().id() == object {
                    return Err(GitError::NoChange);
                }

                entry.filemode()
            } else {
                // non-executable file mode
                0o100644
            };
            tree.insert(filename.bytes(), object, filemode)?;
            Ok(())
        } else {
            let first = path.pop_first().unwrap();
            let mut subtree_builder = if let Some(entry) = tree.get(first.bytes())? {
                if let Some(subtree) = entry.to_object(&self.repo.repo)?.as_tree() {
                    self.repo.repo.treebuilder(Some(subtree))?
                } else {
                    return Err(GitError::CannotCreate);
                }
            } else {
                self.repo.repo.treebuilder(None)?
            };

            self.add_to_tree(&mut subtree_builder, path, object)?;

            let subtree_oid = subtree_builder.write()?;
            tree.insert(first.bytes(), subtree_oid, 0o040000)?;

            Ok(())
        }
    }

    pub fn remove(&self, message: &str) -> Result<(), GitError> {
        if self.is_root() {
            // I decided to not allow removal of the root dir because this is only very rarely the
            // action you want to take and it would require changing some implementation details.
            //
            // TODO better error
            return Err(GitError::NotFound);
        }
        let head = self.repo.head()?;
        let head_tree = head.tree()?;

        let mut tree_builder = self.repo.repo.treebuilder(Some(&head_tree))?;

        tree_builder = if self.remove_from_tree(&mut tree_builder, self.path.clone())? {
            self.repo.repo.treebuilder(None)?
        } else {
            tree_builder
        };

        // TODO deduplicate commit creation with Self::edit
        let tree_oid = tree_builder.write()?;
        let new_tree = self.repo.repo.find_tree(tree_oid)?;

        let signature = Signature::now("smeagol", "smeagol@smeagol")?;

        self.repo.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &new_tree,
            &[&head],
        )?;

        Ok(())
    }

    fn remove_from_tree(&self, tree: &mut TreeBuilder, mut path: Path) -> Result<bool, GitError> {
        assert!(!path.is_empty());

        if path.segments().count() == 1 {
            if tree.get(path.bytes())?.is_none() {
                Err(GitError::NotFound)
            } else {
                tree.remove(path.bytes())?;
                // TODO could returning true happen earlier?
                Ok(tree.len() == 0)
            }
        } else {
            let first = path.pop_first().unwrap();

            let mut subtree_builder = if let Some(entry) = tree.get(first.bytes())? {
                if let Some(subtree) = entry.to_object(&self.repo.repo)?.as_tree() {
                    self.repo.repo.treebuilder(Some(subtree))?
                } else {
                    return Err(GitError::NotFound);
                }
            } else {
                return Err(GitError::NotFound);
            };

            if self.remove_from_tree(&mut subtree_builder, path)? {
                if tree.len() == 1 {
                    return Ok(true);
                }
            }
            let subtree_oid = subtree_builder.write()?;
            // TODO constants for git mode constants...
            tree.insert(first.bytes(), subtree_oid, 0o040000)?;

            Ok(false)
        }
    }
}

#[derive(Debug)]
pub enum GitError {
    Git(git2::Error),
    IO(io::Error),
    NotFound,
    NoParent,
    IsDir,
    IsFile,
    CannotCreate,
    NoChange,
}
impl std::error::Error for GitError {}
impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            &GitError::Git(ref err) => write!(f, "Git error: {}", err),
            &GitError::IO(ref err) => write!(f, "IO error: {}", err),
            &GitError::NotFound => write!(f, "Not found"),
            &GitError::NoParent => write!(f, "No parent"),
            &GitError::IsDir => write!(f, "Is directory"),
            &GitError::IsFile => write!(f, "Is file"),
            &GitError::CannotCreate => write!(f, "Cannot create file at that location"),
            &GitError::NoChange => write!(f, "The file has not changed."),
        }
    }
}
impl From<git2::Error> for GitError {
    fn from(err: git2::Error) -> Self {
        GitError::Git(err)
    }
}
impl From<io::Error> for GitError {
    fn from(err: io::Error) -> Self {
        GitError::IO(err)
    }
}

#[cfg(test)]
mod tests {
    use crate::git::GitError;
    use crate::{GitRepository, Path};
    use tempdir::TempDir;

    #[test]
    fn automatic_repo_creation() {
        let tmp_dir = TempDir::new("smeagol").unwrap();
        let path = tmp_dir.path();

        let _repo = GitRepository::new(path).unwrap();

        assert!(path.exists());
        assert!(path.join("HEAD").exists());
        assert!(path.is_dir());
    }

    #[test]
    fn root_always_exists() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::new();
        let item = repo.item(path).unwrap();

        assert!(item.exists().unwrap());
        assert!(item.is_dir().unwrap());
        assert!(!item.is_file().unwrap());
    }

    #[test]
    fn edit_file() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("index.md".to_string());
        let item = repo.item(path).unwrap();

        assert!(!item.exists().unwrap());
        assert!(item.can_exist().unwrap());
        match item.content() {
            Err(GitError::NotFound) => {}
            _ => panic!(),
        }

        let file_content = "This is a file.".bytes().collect::<Vec<u8>>();
        item.edit(&file_content, "Commit message").unwrap();

        assert!(item.is_file().unwrap());
        assert!(!item.is_dir().unwrap());
        assert_eq!(item.content().unwrap(), file_content);

        let file_content = "This is an edited file.".bytes().collect::<Vec<u8>>();
        item.edit(&file_content, "Commit message 2").unwrap();

        assert_eq!(item.content().unwrap(), file_content);
    }

    #[test]
    fn edit_file_dir() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("test/index.md".to_string());
        let item = repo.item(path).unwrap();
        let dir_item = item.parent().unwrap();

        assert!(!dir_item.exists().unwrap());
        assert!(!item.exists().unwrap());
        assert!(dir_item.can_exist().unwrap());
        assert!(item.can_exist().unwrap());
        match item.content() {
            Err(GitError::NotFound) => {}
            _ => panic!(),
        }

        let file_content = "This is a file.".bytes().collect::<Vec<u8>>();
        item.edit(&file_content, "Commit message").unwrap();

        assert!(dir_item.is_dir().unwrap());
        assert!(!dir_item.is_file().unwrap());
        assert!(item.is_file().unwrap());
        assert!(!item.is_dir().unwrap());
        assert_eq!(item.content().unwrap(), file_content);
    }

    #[test]
    fn remove_only_file_in_repo() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("index.md".to_string());
        let item = repo.item(path).unwrap();

        item.edit("content".as_bytes(), "Commit message").unwrap();
        assert!(item.exists().unwrap());

        item.remove("Commit message").unwrap();
        assert!(!item.exists().unwrap());
    }
    #[test]
    fn remove_single_file() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path1 = Path::from("index1.md".to_string());
        let item1 = repo.item(path1).unwrap();
        let path2 = Path::from("index2.md".to_string());
        let item2 = repo.item(path2).unwrap();

        item1.edit("content1".as_bytes(), "Commit message").unwrap();
        assert!(item1.exists().unwrap());
        item2.edit("content2".as_bytes(), "Commit message").unwrap();
        assert!(item2.exists().unwrap());

        item1.remove("Commit message").unwrap();
        assert!(!item1.exists().unwrap());
        assert!(item2.exists().unwrap());
    }

    #[test]
    fn remove_only_file_in_dir_and_repo() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("test/index.md".to_string());
        let item = repo.item(path).unwrap();
        let dir_item = item.parent().unwrap();

        item.edit("content".as_bytes(), "Commit message").unwrap();
        assert!(item.exists().unwrap());

        item.remove("Commit message").unwrap();
        assert!(!item.exists().unwrap());
        assert!(!dir_item.exists().unwrap());
    }
    #[test]
    fn remove_single_file_in_dir() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path1 = Path::from("test/index1.md".to_string());
        let item1 = repo.item(path1).unwrap();
        let path2 = Path::from("test/index2.md".to_string());
        let item2 = repo.item(path2).unwrap();
        let dir_item = item1.parent().unwrap();

        item1.edit("content1".as_bytes(), "Commit message").unwrap();
        assert!(item1.exists().unwrap());
        item2.edit("content2".as_bytes(), "Commit message").unwrap();
        assert!(item2.exists().unwrap());

        item1.remove("Commit message").unwrap();
        assert!(!item1.exists().unwrap());
        assert!(item2.exists().unwrap());
        assert!(dir_item.exists().unwrap());
    }
    #[test]
    fn remove_only_file_in_dir_but_not_in_repo() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path1 = Path::from("test/index1.md".to_string());
        let item1 = repo.item(path1).unwrap();
        let path2 = Path::from("index2.md".to_string());
        let item2 = repo.item(path2).unwrap();
        let dir_item = item1.parent().unwrap();

        item1.edit("content1".as_bytes(), "Commit message").unwrap();
        assert!(item1.exists().unwrap());
        item2.edit("content2".as_bytes(), "Commit message").unwrap();
        assert!(item2.exists().unwrap());

        item1.remove("Commit message").unwrap();
        assert!(!item1.exists().unwrap());
        assert!(item2.exists().unwrap());
        assert!(dir_item.exists().unwrap());
    }

    #[test]
    fn remove_only_dir_in_repo() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("test/index.md".to_string());
        let item = repo.item(path).unwrap();
        let dir_item = item.parent().unwrap();

        item.edit("content1".as_bytes(), "Commit message").unwrap();
        assert!(item.exists().unwrap());

        dir_item.remove("Commit message").unwrap();
        assert!(!item.exists().unwrap());
        assert!(!dir_item.exists().unwrap());
    }

    #[test]
    fn can_exist1() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("index.md".to_string());
        let item = repo.item(path).unwrap();

        item.edit(&vec![], "commit").unwrap();

        let path2 = Path::from("index.md/something.md".to_string());
        let item2 = repo.item(path2).unwrap();
        assert!(!item2.can_exist().unwrap());

        match item2.content() {
            Err(GitError::NotFound) => {}
            _ => panic!(),
        }
        match item2.edit(&vec![], "commit") {
            Err(GitError::CannotCreate) => {}
            _ => panic!(),
        }
    }
    #[test]
    fn can_exist2() {
        let tmp = TempDir::new("smeagol").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();

        let path = Path::from("test/index.md".to_string());
        let item = repo.item(path).unwrap();

        item.edit(&vec![], "commit").unwrap();

        let path2 = Path::from("test/index.md/something.md".to_string());
        let item2 = repo.item(path2).unwrap();
        assert!(!item2.can_exist().unwrap());

        match item2.content() {
            Err(GitError::NotFound) => {}
            _ => panic!(),
        }
        match item2.edit(&vec![], "commit") {
            Err(GitError::CannotCreate) => {}
            _ => panic!(),
        }
    }
}
