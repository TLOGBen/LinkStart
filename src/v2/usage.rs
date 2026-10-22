//! Caller-first examples; Browser/CLI adapters map these same calls over daemon transport.
use super::*;

pub async fn owner_and_two_frontends(
    runtime: &Runtime,
    manager_name: String,
    terminal: OwnedTerminalSpec,
    provider: ProviderSpec,
) -> Result<()> {
    let owner = runtime.local_owner().await?;
    let bootstrap = owner
        .create_space(
            RequestId("create-1".into()),
            SpaceSpec {
                name: "release".into(),
                administrator_name: "local human".into(),
                first_manager_name: Some(manager_name),
                presets: vec![
                    Preset::FrontendLink,
                    Preset::PersistentTerminal,
                    Preset::AsyncCollaboration,
                ],
            },
        )
        .await?;
    let cli = runtime
        .join(
            &bootstrap
                .manager
                .as_ref()
                .ok_or(Error::Unavailable)?
                .credential,
            bootstrap.space.clone(),
        )
        .await?;
    let invite = cli
        .invite(RequestId("invite-1".into()), vec![Role::Implementer])
        .await?;
    let browser_identity = runtime.register(invite, "browser".into()).await?;
    let browser = runtime
        .join(&browser_identity.credential, bootstrap.space.clone())
        .await?;
    let terminal = cli
        .create_terminal(RequestId("pty-1".into()), terminal)
        .await?;
    let provider = cli
        .connect_provider(RequestId("sdk-1".into()), provider)
        .await?;
    let _same_native_identity = provider.session().await?;
    let _targets = browser.endpoints().await?;
    cli.grant_endpoint(
        RequestId("grant-terminal".into()),
        browser_identity.identity.clone(),
        terminal.id().clone(),
        EndpointRights {
            observe: true,
            send: true,
            control: false,
        },
    )
    .await?;
    cli.grant_endpoint(
        RequestId("grant-provider".into()),
        browser_identity.identity,
        provider.id().clone(),
        EndpointRights {
            observe: true,
            send: true,
            control: false,
        },
    )
    .await?;
    let mirror = browser.terminal(terminal.session().await?).await?;
    let mut output = mirror.output(None).await?;
    let writer = terminal.control(ControlChoice::Acquire).await?;
    let input = writer.write(b"status\r".to_vec()).await?;
    // Unknown raw input is shown for manual reconciliation, never automatically resent.
    let _receipt = provider
        .send(RequestId("prompt-1".into()), "summarize".into())
        .await?;
    let browser_provider = browser.provider(provider.session().await?).await?;
    let mut events = browser_provider.events(None).await?;
    let _browser_receipt = browser_provider
        .send(RequestId("browser-prompt-1".into()), "continue".into())
        .await?;
    let (_raw, _structured) = (output.next().await?, events.next().await?);
    let _delivery = input;
    cli.heartbeat().await?;
    cli.release().await?; // owned child continues; every child handle is now fenced.
    Ok(())
}

pub async fn board_rejoin_and_management(
    runtime: &Runtime,
    credential: &Credential,
    space: SpaceId,
    topic: TopicId,
    manager: IdentityId,
    member: IdentityId,
    endpoint: EndpointId,
    session: SessionIdentity,
) -> Result<()> {
    let joined = runtime.join(credential, space.clone()).await?;
    let board = joined.topic(topic.clone()).await?;
    let request = RequestId("offline-post-7".into());
    let receipt = board.post(request.clone(), None, "ready".into()).await?;
    let _same_receipt = board.post(request, None, "ready".into()).await?;
    board
        .post(
            RequestId("reply-7".into()),
            Some(receipt.message),
            "received".into(),
        )
        .await?;
    let page = board.read(None, 40).await?;
    let _next = board.read(page.next, 40).await?;
    let old_control = joined
        .terminal(session)
        .await?
        .control(ControlChoice::Acquire)
        .await?;
    joined.release().await?;
    let fresh = runtime.join(credential, space).await?;
    // Runtime must return StaleAttachment even though old_control remains a Rust value.
    let _rejected = old_control.resize(120, 35).await;
    fresh
        .set_roles(RequestId("delegate-1".into()), manager, vec![Role::Manager])
        .await?;
    fresh
        .grant_endpoint(
            RequestId("grant-1".into()),
            member,
            endpoint,
            EndpointRights {
                observe: true,
                send: true,
                control: false,
            },
        )
        .await?;
    let mut updates = fresh.topic(topic).await?.subscribe(None).await?;
    let _event = updates.next().await?;
    Ok(())
}

pub async fn human_startup_settings_shutdown(
    runtime: &Runtime,
    credential: &Credential,
    settings: Settings,
    terminal: OwnedTerminalSpec,
    selected: CandidateId,
    space: SpaceId,
) -> Result<ShutdownProgress> {
    let owner = runtime.local_owner().await?;
    let _candidates = runtime.startup_discovery().await?;
    let joined = runtime.join(credential, space).await?;
    let verified = runtime.probe(&owner, selected).await?;
    let _verified_join = joined
        .attach_candidate(RequestId("attach-candidate".into()), verified)
        .await?;
    let _discoverable = joined
        .create_terminal(RequestId("human-create".into()), terminal)
        .await?;
    let _pool = joined.pool().await?;
    let plan = owner.preview_settings(settings).await?;
    owner
        .apply_settings(RequestId("settings-1".into()), plan)
        .await?;
    let ticket = owner.prepare_shutdown().await?;
    let _impact_for_confirmation = ticket.impact();
    let job = owner
        .confirm_shutdown(RequestId("stop-1".into()), ticket)
        .await?;
    // Pending/Failed require explicit human decision; UI renders Stopped only after exit proof.
    let progress = owner.shutdown_progress(&job).await?;
    let _external_exit_witness = owner.observe_exit(&job).await?;
    Ok(progress) // owner.force_shutdown(...) is a separate explicit action.
}

/// Cancellation needs a turn observed from this provider, never an unrelated raw string.
pub async fn cancel_observed_turn(
    provider: &Provider,
    wait_budget: std::time::Duration,
) -> Result<()> {
    // Subscribe before submitting so a fast TurnStarted cannot fall between the two calls.
    let mut events = provider.events(None).await?;
    let receipt = provider
        .send(RequestId("work-to-cancel".into()), "work".into())
        .await?;
    let submitted = receipt.request.clone();
    match &receipt.state {
        DeliveryState::Failed(reason) => {
            return Err(Error::DeliveryFailed {
                request: submitted,
                reason: reason.clone(),
            })
        }
        DeliveryState::Unknown => return Err(Error::UnknownEffect(submitted)),
        DeliveryState::ModelCompleted => return Err(Error::NoActiveTurn(submitted)),
        _ => {}
    }
    let turn = if let Some(turn) = receipt.turn {
        turn
    } else {
        tokio::time::timeout(wait_budget, async {
            loop {
                match events.next().await? {
                    Some(OutputFrame {
                        event: StructuredEvent::TurnStarted { request, turn },
                        ..
                    }) if request.0 == submitted.0 => break Ok(turn),
                    Some(OutputFrame {
                        event: StructuredEvent::Delivery { request, state },
                        ..
                    }) if request.0 == submitted.0 => match state {
                        DeliveryState::Failed(reason) => {
                            return Err(Error::DeliveryFailed {
                                request: submitted.clone(),
                                reason,
                            })
                        }
                        DeliveryState::Unknown => {
                            return Err(Error::UnknownEffect(submitted.clone()))
                        }
                        DeliveryState::ModelCompleted => {
                            return Err(Error::NoActiveTurn(submitted.clone()))
                        }
                        _ => continue,
                    },
                    Some(OutputFrame {
                        event: StructuredEvent::TurnCompleted { request, .. },
                        ..
                    }) if request.0 == submitted.0 => {
                        return Err(Error::NoActiveTurn(submitted.clone()))
                    }
                    Some(OutputFrame {
                        event: StructuredEvent::Gap { .. },
                        ..
                    }) => return Err(Error::UnknownEffect(submitted.clone())),
                    Some(_) => continue,
                    None => return Err(Error::Unavailable),
                }
            }
        })
        .await
        .map_err(|_| Error::TurnNotObserved(submitted.clone()))??
    };
    provider
        .cancel(RequestId("cancel-observed".into()), turn)
        .await?;
    Ok(())
}

pub async fn configure_autojoin(
    owner: &Owner,
    mut settings: Settings,
    space: SpaceId,
    identity: IdentityId,
) -> Result<()> {
    settings.sources.push(AuthorizedSource {
        adapter: "registered-sdk".into(),
        locator: "adapter:work".into(),
        credential_slot: "provider-secret-slot".into(),
        autojoin: Autojoin::Join {
            space,
            identity,
            identity_credential_slot: "joining-identity-slot".into(),
        },
    });
    let plan = owner.preview_settings(settings).await?;
    owner
        .apply_settings(RequestId("autojoin-config".into()), plan)
        .await?;
    Ok(())
}

/// Compile-only example: CLI submits an explicit action and comment, never parsed prose.
pub async fn claim_with_plan(
    space: &Space,
    topic_id: TopicId,
    request: RequestId,
) -> Result<ForumReceipt> {
    let topic = space.topic(topic_id).await?;
    let snapshot = topic.snapshot().await?;
    topic
        .act(
            request,
            ForumCommand {
                expected_revision: snapshot.revision,
                artifact_ref: None,
                expected_submission: None,
                action: ForumAction::Claim,
                reply_to: None,
                text: "I will verify idle delivery and attach the evidence.".into(),
            },
        )
        .await // TopicConflict means reread and decide; never silently retry a new revision.
}

pub async fn space_without_lead(runtime: &Runtime) -> Result<Space> {
    let bootstrap = runtime
        .local_owner()
        .await?
        .create_space(
            RequestId("human-space".into()),
            SpaceSpec {
                name: "Async team".into(),
                administrator_name: "Local administrator".into(),
                first_manager_name: None,
                presets: vec![Preset::AsyncCollaboration],
            },
        )
        .await?;
    runtime
        .join(&bootstrap.administrator.credential, bootstrap.space)
        .await
}
