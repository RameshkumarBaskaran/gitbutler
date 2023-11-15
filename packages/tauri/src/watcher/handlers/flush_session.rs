use anyhow::{Context, Result};
use tauri::{AppHandle, Manager};

use crate::{
    gb_repository, paths::DataDir, project_repository, projects, projects::ProjectId, sessions,
    users, virtual_branches,
};

use super::events;

#[derive(Clone)]
pub struct Handler {
    local_data_dir: DataDir,
    project_store: projects::Controller,
    vbrach_controller: virtual_branches::Controller,
    users: users::Controller,
}

impl TryFrom<&AppHandle> for Handler {
    type Error = anyhow::Error;

    fn try_from(value: &AppHandle) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            local_data_dir: DataDir::try_from(value)?,
            project_store: projects::Controller::try_from(value)?,
            vbrach_controller: value
                .state::<virtual_branches::Controller>()
                .inner()
                .clone(),
            users: users::Controller::from(value),
        })
    }
}

impl Handler {
    pub fn handle(
        &self,
        project_id: &ProjectId,
        session: &sessions::Session,
    ) -> Result<Vec<events::Event>> {
        let project = self
            .project_store
            .get(project_id)
            .context("failed to get project")?;

        let user = self.users.get_user()?;
        let project_repository =
            project_repository::Repository::open(&project).context("failed to open repository")?;
        let gb_repo = gb_repository::Repository::open(
            &self.local_data_dir,
            &project_repository,
            user.as_ref(),
        )
        .context("failed to open repository")?;

        match futures::executor::block_on(async {
            self.vbrach_controller.flush_vbranches(*project_id).await
        }) {
            Ok(()) => Ok(()),
            Err(virtual_branches::ControllerError::Verify(error)) => {
                tracing::warn!("failed to flush virtual branches: {:#}", error);
                Ok(())
            }
            Err(error) => Err(error),
        }?;

        let session = gb_repo
            .flush_session(&project_repository, session, user.as_ref())
            .context(format!("failed to flush session {}", session.id))?;

        Ok(vec![
            events::Event::Session(*project_id, session),
            events::Event::PushGitbutlerData(*project_id),
            events::Event::PushProjectToGitbutler(*project_id),
        ])
    }
}
