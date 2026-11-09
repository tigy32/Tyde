use protocol::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::HashMap, path::PathBuf};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(crate) struct BoardHandle(mpsc::UnboundedSender<Request>);

enum Request {
    Apply {
        command: BoardCommandPayload,
        live: Vec<(ProjectId, AgentStartPayload)>,
        actor: BoardActor,
        reply: oneshot::Sender<Result<BoardStatePayload, BoardErrorPayload>>,
    },
    Settle(
        AgentStartPayload,
        Option<ProjectId>,
        oneshot::Sender<Result<(), String>>,
    ),
    Sections(oneshot::Sender<Result<HashMap<ProjectId, String>, String>>),
    Rotate {
        old: AgentId,
        new: AgentId,
        session: SessionId,
        old_session: SessionId,
        summary: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

fn storage(error: impl std::fmt::Display) -> String {
    format!("board storage: {error}")
}
fn failure(
    project_id: &ProjectId,
    code: BoardErrorCode,
    message: impl Into<String>,
) -> BoardErrorPayload {
    BoardErrorPayload {
        project_id: project_id.clone(),
        code,
        message: message.into(),
    }
}

pub(crate) fn command_project(command: &BoardCommandPayload) -> &ProjectId {
    match command {
        BoardCommandPayload::Read(p) => &p.project_id,
        BoardCommandPayload::Move(p) => &p.project_id,
        BoardCommandPayload::Lanes(p) => &p.project_id,
        BoardCommandPayload::Remove { project_id, .. } => project_id,
    }
}

impl BoardHandle {
    pub(crate) fn start(path: PathBuf) -> Result<Self, String> {
        let mut connection = Connection::open(path).map_err(storage)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS board_versions (project_id TEXT PRIMARY KEY, version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS board_lanes (project_id TEXT NOT NULL, id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(project_id,id));
            CREATE TABLE IF NOT EXISTS board_cards (project_id TEXT NOT NULL, id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, session_id TEXT, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS board_cards_project ON board_cards(project_id);
            CREATE UNIQUE INDEX IF NOT EXISTS board_cards_agent ON board_cards(agent_id);
            CREATE UNIQUE INDEX IF NOT EXISTS board_cards_session ON board_cards(session_id);
            CREATE TABLE IF NOT EXISTS board_moves (card_id TEXT NOT NULL, seq INTEGER NOT NULL, record TEXT NOT NULL, PRIMARY KEY(card_id,seq));").map_err(storage)?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("board-store".into())
            .spawn(move || {
                while let Some(request) = rx.blocking_recv() {
                    match request {
                        Request::Apply {
                            command,
                            live,
                            actor,
                            reply,
                        } => {
                            let id = command_project(&command).clone();
                            let result = apply(&mut connection, command, live, actor)
                                .map_err(|error| failure(&id, BoardErrorCode::Storage, error))
                                .and_then(|result| result);
                            let _ = reply.send(result);
                        }
                        Request::Settle(start, owner, reply) => {
                            let result = (|| {
                                let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(storage)?;
                                if let Some(owner) = owner { reconcile(&transaction,&[(owner,start.clone())])?; }
                                let mut statement = transaction
                                    .prepare("SELECT project_id,record FROM board_cards WHERE agent_id=? OR session_id=?")
                                    .map_err(storage)?;
                                let rows = statement
                                    .query_map(params![start.agent_id.0,start.session_id.as_ref().map(|id| &id.0)], |row| {
                                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                                    })
                                    .map_err(storage)?
                                    .collect::<Result<Vec<_>, _>>()
                                    .map_err(storage)?;
                                drop(statement);
                                for (project, record) in rows {
                                    let mut card: BoardCard =
                                        serde_json::from_str(&record).map_err(storage)?;
                                    if card.agent_id != start.agent_id && !start.session_id.as_ref().is_some_and(|session| card.session_id.as_ref() == Some(session)) {
                                        return Err("board card identity index disagrees with its record".into());
                                    }
                                    card.agent_id = start.agent_id.clone();
                                    card.title = start.name.clone();
                                    card.session_id = start.session_id.clone();
                                    persist_card(&transaction, &ProjectId(project), &card)?;
                                }
                                transaction.commit().map_err(storage)
                            })();
                            let _ = reply.send(result);
                        }
                        Request::Sections(reply) => {
                            let _ = reply.send(sections(&connection));
                        }
                        Request::Rotate {
                            old,
                            new,
                            session,
                            old_session,
                            summary,
                            reply,
                        } => {
                            let _ = reply.send(rotate(
                                &mut connection,
                                old,
                                new,
                                old_session,
                                session,
                                summary,
                            ));
                        }
                    }
                }
            })
            .map_err(storage)?;
        Ok(Self(tx))
    }
    pub(crate) async fn apply(
        &self,
        command: BoardCommandPayload,
        live: Vec<(ProjectId, AgentStartPayload)>,
        actor: BoardActor,
    ) -> Result<BoardStatePayload, BoardErrorPayload> {
        let id = command_project(&command).clone();
        let (reply, rx) = oneshot::channel();
        self.0
            .send(Request::Apply {
                command,
                live,
                actor,
                reply,
            })
            .map_err(|_| failure(&id, BoardErrorCode::Storage, "board actor stopped"))?;
        rx.await
            .map_err(|_| failure(&id, BoardErrorCode::Storage, "board actor stopped"))?
    }
    pub(crate) async fn settle_card(
        &self,
        start: AgentStartPayload,
        owner: Option<ProjectId>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.0
            .send(Request::Settle(start, owner, reply))
            .map_err(storage)?;
        rx.await.map_err(storage)?
    }
    pub(crate) async fn sections(&self) -> Result<HashMap<ProjectId, String>, String> {
        let (reply, rx) = oneshot::channel();
        self.0.send(Request::Sections(reply)).map_err(storage)?;
        rx.await.map_err(storage)?
    }
    pub(crate) async fn rotate(
        &self,
        old: AgentId,
        new: AgentId,
        old_session: SessionId,
        session: SessionId,
        summary: String,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.0
            .send(Request::Rotate {
                old,
                new,
                session,
                old_session,
                summary,
                reply,
            })
            .map_err(storage)?;
        rx.await.map_err(storage)?
    }
}

fn default_lanes() -> Vec<BoardLane> {
    ["Active", "Done"]
        .into_iter()
        .enumerate()
        .map(|(position, name)| BoardLane {
            id: BoardLaneId(format!("default-{}", name.to_lowercase())),
            name: name.into(),
            position: position as u64,
            instructions: None,
        })
        .collect()
}
fn records<T: serde::de::DeserializeOwned>(
    db: &Connection,
    sql: &str,
    id: &str,
) -> Result<Vec<T>, String> {
    let mut statement = db.prepare(sql).map_err(storage)?;
    statement
        .query_map([id], |row| row.get::<_, String>(0))
        .map_err(storage)?
        .map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(storage))
        .collect()
}
fn read(
    db: &Connection,
    id: &ProjectId,
    live: Vec<AgentStartPayload>,
) -> Result<BoardStatePayload, String> {
    let version = db
        .query_row(
            "SELECT version FROM board_versions WHERE project_id=?",
            [&id.0],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(storage)?
        .unwrap_or(0);
    let version = u64::try_from(version).map_err(storage)?;
    let mut lanes: Vec<BoardLane> = records(
        db,
        "SELECT record FROM board_lanes WHERE project_id=?",
        &id.0,
    )?;
    let customized = version > 0;
    if lanes.is_empty() {
        lanes = default_lanes();
    }
    lanes.sort_by_key(|lane| lane.position);
    let mut cards: Vec<BoardCard> = records(
        db,
        "SELECT record FROM board_cards WHERE project_id=?",
        &id.0,
    )?;
    for card in &mut cards {
        card.history = records(
            db,
            "SELECT record FROM board_moves WHERE card_id=? ORDER BY seq",
            &card.id.0,
        )?;
        if let Some(start) = live.iter().find(|start| start.agent_id == card.agent_id) {
            card.title = start.name.clone();
            card.session_id = start.session_id.clone();
        }
    }
    for start in live.clone() {
        let card_id = identity(db, &start)?;
        if !cards
            .iter()
            .any(|card| card.agent_id == start.agent_id || card.id == card_id)
        {
            cards.push(BoardCard {
                retained: false,
                id: card_id,
                agent_id: start.agent_id,
                session_id: start.session_id,
                title: start.name,
                location: BoardLocation::Lane {
                    project_id: id.clone(),
                    lane_id: lanes[0].id.clone(),
                },
                position: 0,
                version: 0,
                history: vec![],
            });
        }
    }
    cards.sort_by(|a, b| a.position.cmp(&b.position).then(a.id.0.cmp(&b.id.0)));
    let navigation = navigation(db, &cards, &live)?;
    Ok(BoardStatePayload {
        navigation,
        live_agents: live,
        project_id: id.clone(),
        version,
        customized,
        lanes,
        cards,
    })
}
fn navigation(
    db: &Connection,
    cards: &[BoardCard],
    live: &[AgentStartPayload],
) -> Result<Vec<BoardCardNavigation>, String> {
    cards
        .iter()
        .map(|card| {
            let target = if live.iter().any(|agent| agent.agent_id == card.agent_id) {
                BoardNavigationTarget::Live {
                    agent_id: card.agent_id.clone(),
                }
            } else if let Some(session) = &card.session_id {
                let record: Option<String> = db
                    .query_row(
                        "SELECT record FROM sessions WHERE id=?",
                        [&session.0],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage)?;
                match record {
                    Some(record) => {
                        let record: crate::store::session::SessionRecord =
                            serde_json::from_str(&record).map_err(storage)?;
                        BoardNavigationTarget::Saved {
                            session_id: session.clone(),
                            backend_kind: record.backend_kind,
                            project_id: record.project_id,
                        }
                    }
                    None => BoardNavigationTarget::Unavailable,
                }
            } else {
                BoardNavigationTarget::Unavailable
            };
            Ok(BoardCardNavigation {
                card_id: card.id.clone(),
                target,
            })
        })
        .collect()
}
fn lane_of(card: &BoardCard, cards: &[BoardCard]) -> Result<BoardLaneId, String> {
    match &card.location {
        BoardLocation::Lane { lane_id, .. } => Ok(lane_id.clone()),
        BoardLocation::Parent { card_id } => match cards
            .iter()
            .find(|parent| &parent.id == card_id)
            .map(|parent| &parent.location)
        {
            Some(BoardLocation::Lane { lane_id, .. }) => Ok(lane_id.clone()),
            _ => Err("board parent must be an ordinary card".into()),
        },
    }
}
fn persist_card(db: &Connection, project: &ProjectId, card: &BoardCard) -> Result<(), String> {
    let mut row = card.clone();
    row.retained = true;
    row.history.clear();
    db.execute("INSERT INTO board_cards(project_id,id,agent_id,session_id,record) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,agent_id=excluded.agent_id,session_id=excluded.session_id,record=excluded.record", params![project.0,card.id.0,card.agent_id.0,card.session_id.as_ref().map(|id| &id.0),serde_json::to_string(&row).map_err(storage)?]).map_err(storage)?;
    for movement in &card.history {
        let existing: Option<String> = db
            .query_row(
                "SELECT record FROM board_moves WHERE card_id=? AND seq=?",
                params![card.id.0, i64::try_from(movement.seq).map_err(storage)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(existing) = existing {
            let existing: BoardMove = serde_json::from_str(&existing).map_err(storage)?;
            if existing != *movement {
                return Err("board move history conflict".into());
            }
            continue;
        }
        db.execute(
            "INSERT INTO board_moves(card_id,seq,record) VALUES(?,?,?)",
            params![
                card.id.0,
                i64::try_from(movement.seq).map_err(storage)?,
                serde_json::to_string(movement).map_err(storage)?
            ],
        )
        .map_err(storage)?;
    }
    Ok(())
}
fn record_move(
    card: &mut BoardCard,
    to: BoardLocation,
    actor: &BoardActor,
    note: Option<String>,
) -> Result<(), String> {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(storage)?
        .as_millis() as u64;
    card.history.push(BoardMove {
        seq: card.version + 1,
        from: card.location.clone(),
        to: to.clone(),
        actor: actor.clone(),
        note,
        at,
    });
    card.retained = true;
    card.version += 1;
    card.location = to;
    Ok(())
}
fn apply(
    db: &mut Connection,
    command: BoardCommandPayload,
    live: Vec<(ProjectId, AgentStartPayload)>,
    actor: BoardActor,
) -> Result<Result<BoardStatePayload, BoardErrorPayload>, String> {
    let id = command_project(&command).clone();
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage)?;
    reconcile(&tx, &live)?;
    let live = live
        .into_iter()
        .filter_map(|(project, start)| (project == id).then_some(start))
        .collect();
    let mut board = read(&tx, &id, live)?;
    let invalid = |message: &str| Ok(Err(failure(&id, BoardErrorCode::InvalidInput, message)));
    match command {
        BoardCommandPayload::Read(_) => {
            tx.commit().map_err(storage)?;
            return Ok(Ok(board));
        }
        BoardCommandPayload::Lanes(p) => {
            if p.expected_version != board.version {
                return Ok(Err(failure(
                    &id,
                    BoardErrorCode::StaleVersion,
                    "Lanes changed elsewhere. Reopen to edit.",
                )));
            }
            if p.lanes.is_empty() {
                return invalid("A project needs at least one lane");
            }
            let mut names = std::collections::HashSet::new();
            let mut ids = std::collections::HashSet::new();
            let mut positions = std::collections::HashSet::new();
            for lane in &p.lanes {
                if lane.name.trim().is_empty() {
                    return invalid("Lane names can't be blank");
                }
                if !names.insert(lane.name.trim().to_lowercase()) {
                    return invalid("Lane names must be unique");
                }
                if lane.id.0.is_empty()
                    || !ids.insert(lane.id.clone())
                    || !positions.insert(lane.position)
                {
                    return invalid("lane ids and positions must be non-empty and unique");
                }
            }
            let mut lanes = p.lanes;
            for lane in &mut lanes {
                lane.name = lane.name.trim().to_owned();
                if lane
                    .instructions
                    .as_ref()
                    .is_some_and(|text| text.trim().is_empty())
                {
                    lane.instructions = None;
                }
            }
            lanes.sort_by_key(|lane| lane.position);
            if lanes == board.lanes {
                return Ok(Ok(board));
            }
            for card in &mut board.cards {
                if !card.retained {
                    card.location = BoardLocation::Lane {
                        project_id: id.clone(),
                        lane_id: lanes[0].id.clone(),
                    };
                    continue;
                }
                if let BoardLocation::Lane { lane_id, .. } = &card.location
                    && !ids.contains(lane_id)
                {
                    record_move(
                        card,
                        BoardLocation::Lane {
                            project_id: id.clone(),
                            lane_id: lanes[0].id.clone(),
                        },
                        &actor,
                        None,
                    )?;
                    persist_card(&tx, &id, card)?;
                }
            }
            tx.execute("DELETE FROM board_lanes WHERE project_id=?", [&id.0])
                .map_err(storage)?;
            for lane in &lanes {
                tx.execute(
                    "INSERT INTO board_lanes(project_id,id,record) VALUES(?,?,?)",
                    params![
                        id.0,
                        lane.id.0,
                        serde_json::to_string(lane).map_err(storage)?
                    ],
                )
                .map_err(storage)?;
            }
            board.lanes = lanes;
            board.version += 1;
            board.customized = true;
            tx.execute("INSERT INTO board_versions VALUES(?,?) ON CONFLICT(project_id) DO UPDATE SET version=excluded.version", params![id.0,i64::try_from(board.version).map_err(storage)?]).map_err(storage)?;
        }
        BoardCommandPayload::Move(p) => {
            let Some(index) = board.cards.iter().position(|card| card.id == p.card_id) else {
                return Ok(Err(failure(
                    &id,
                    BoardErrorCode::NotFound,
                    "card not found",
                )));
            };
            if board.cards[index].version != p.expected_version {
                return Ok(Err(failure(
                    &id,
                    BoardErrorCode::StaleVersion,
                    "This card changed elsewhere. Try again.",
                )));
            }
            let location = match p.destination {
                BoardDestination::Lane { name } => {
                    let Some(lane) = board
                        .lanes
                        .iter()
                        .find(|lane| lane.name.trim().to_lowercase() == name.trim().to_lowercase())
                    else {
                        return Ok(Err(failure(
                            &id,
                            BoardErrorCode::NotFound,
                            format!(
                                "unknown lane; lanes: {}",
                                board
                                    .lanes
                                    .iter()
                                    .map(|lane| lane.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        )));
                    };
                    BoardLocation::Lane {
                        project_id: id.clone(),
                        lane_id: lane.id.clone(),
                    }
                }
                BoardDestination::Parent { card_id } => {
                    let Some(parent) = board.cards.iter().find(|card| card.id == card_id) else {
                        return invalid("parent card not found");
                    };
                    if card_id == p.card_id
                        || matches!(parent.location, BoardLocation::Parent { .. })
                        || board.cards.iter().any(|card| {
                            card.location
                                == (BoardLocation::Parent {
                                    card_id: p.card_id.clone(),
                                })
                        })
                    {
                        return invalid("nesting is limited to one level");
                    }
                    // A previously untouched parent becomes durable when first used for nesting.
                    let parent_id = parent.id.clone();
                    let parent = board
                        .cards
                        .iter_mut()
                        .find(|card| card.id == parent_id)
                        .ok_or("parent disappeared")?;
                    parent.retained = true;
                    persist_card(&tx, &id, parent)?;
                    BoardLocation::Parent { card_id }
                }
            };
            let card = &mut board.cards[index];
            record_move(card, location, &actor, p.note)?;
            if let Some(position) = p.position {
                card.position = position;
            }
            persist_card(&tx, &id, card)?;
        }
        BoardCommandPayload::Remove {
            card_id,
            expected_version,
            ..
        } => {
            let Some(index) = board.cards.iter().position(|card| card.id == card_id) else {
                return invalid("card not found");
            };
            if board.cards[index].version != expected_version {
                return Ok(Err(failure(
                    &id,
                    BoardErrorCode::StaleVersion,
                    "This card changed elsewhere. Try again.",
                )));
            }
            if board
                .live_agents
                .iter()
                .any(|agent| agent.agent_id == board.cards[index].agent_id)
            {
                return invalid("close an agent before removing its retained card");
            }
            let lane_id = lane_of(&board.cards[index], &board.cards)?;
            for card in &mut board.cards {
                if card.location
                    == (BoardLocation::Parent {
                        card_id: card_id.clone(),
                    })
                {
                    record_move(
                        card,
                        BoardLocation::Lane {
                            project_id: id.clone(),
                            lane_id: lane_id.clone(),
                        },
                        &actor,
                        None,
                    )?;
                    persist_card(&tx, &id, card)?;
                }
            }
            board.cards.remove(index);
            tx.execute("DELETE FROM board_cards WHERE id=?", [&card_id.0])
                .map_err(storage)?;
            tx.execute("DELETE FROM board_moves WHERE card_id=?", [&card_id.0])
                .map_err(storage)?;
        }
    }
    board.navigation = navigation(&tx, &board.cards, &board.live_agents)?;
    tx.commit().map_err(storage)?;
    Ok(Ok(board))
}
fn sections(db: &Connection) -> Result<HashMap<ProjectId, String>, String> {
    let mut statement = db
        .prepare("SELECT project_id,record FROM board_lanes")
        .map_err(storage)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(storage)?;
    let mut projects: HashMap<ProjectId, Vec<BoardLane>> = HashMap::new();
    for row in rows {
        let (id, record) = row.map_err(storage)?;
        projects
            .entry(ProjectId(id))
            .or_default()
            .push(serde_json::from_str(&record).map_err(storage)?);
    }
    Ok(projects
        .into_iter()
        .filter_map(|(id, mut lanes)| {
            lanes.sort_by_key(|lane| lane.position);
            let entries = lanes
                .into_iter()
                .filter_map(|lane| {
                    lane.instructions
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| format!("{}: {}", lane.name, text))
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then(|| {
                (
                    id,
                    format!(
                        "Lanes. Call tyde_board_read to see which one you're in.\n\n{}",
                        entries.join("\n")
                    ),
                )
            })
        })
        .collect())
}
fn rotate(
    db: &mut Connection,
    old: AgentId,
    new: AgentId,
    old_session: SessionId,
    session: SessionId,
    summary: String,
) -> Result<(), String> {
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage)?;
    let mut card_id =
        session_identity(&tx, &old_session)?.unwrap_or_else(|| BoardCardId(old.0.clone()));
    let mut statement = tx
        .prepare("SELECT project_id,record FROM board_cards WHERE agent_id=? OR session_id=?")
        .map_err(storage)?;
    let rows = statement
        .query_map(params![old.0, old_session.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    drop(statement);
    for (project, record) in rows {
        let mut card: BoardCard = serde_json::from_str(&record).map_err(storage)?;
        if card.agent_id != old && card.session_id.as_ref() != Some(&old_session) {
            return Err("board card identity index disagrees with its record".into());
        }
        card_id = card.id.clone();
        card.agent_id = new.clone();
        card.session_id = Some(session.clone());
        persist_card(&tx, &ProjectId(project), &card)?;
    }
    crate::store::session::mark_compacted_in_transaction(
        &tx,
        &old_session,
        &session,
        summary,
        card_id,
    )?;
    tx.commit().map_err(storage)
}

fn session_identity(db: &Connection, session: &SessionId) -> Result<Option<BoardCardId>, String> {
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM sessions WHERE id=?",
            [&session.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    record
        .map(|record| {
            serde_json::from_str::<crate::store::session::SessionRecord>(&record)
                .map(|record| record.board_card_id)
                .map_err(storage)
        })
        .transpose()
        .map(Option::flatten)
}
fn identity(db: &Connection, start: &AgentStartPayload) -> Result<BoardCardId, String> {
    let identity = match &start.session_id {
        Some(session) => session_identity(db, session)?,
        None => None,
    };
    Ok(identity.unwrap_or_else(|| BoardCardId(start.agent_id.0.clone())))
}

fn reconcile(db: &Connection, live: &[(ProjectId, AgentStartPayload)]) -> Result<(), String> {
    for (project, start) in live {
        let mut statement = db
            .prepare("SELECT project_id,record FROM board_cards WHERE agent_id=? OR session_id=?")
            .map_err(storage)?;
        let rows = statement
            .query_map(
                params![start.agent_id.0, start.session_id.as_ref().map(|id| &id.0)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(statement);
        for (owner, record) in rows {
            let saved: BoardCard = serde_json::from_str(&record).map_err(storage)?;
            let same_session = start
                .session_id
                .as_ref()
                .is_some_and(|session| saved.session_id.as_ref() == Some(session));
            if saved.agent_id != start.agent_id && !same_session {
                continue;
            }
            if saved.agent_id == start.agent_id && owner == project.0 {
                continue;
            }
            let old_board = read(db, &ProjectId(owner), vec![])?;
            let mut card = old_board
                .cards
                .iter()
                .find(|card| card.id == saved.id)
                .cloned()
                .ok_or("retained card missing during reassignment")?;
            card.agent_id = start.agent_id.clone();
            card.session_id = start.session_id.clone();
            card.title = start.name.clone();
            if old_board.project_id != *project {
                let old_lane = lane_of(&card, &old_board.cards)?;
                for mut child in old_board.cards.into_iter().filter(|child| {
                    child.location
                        == (BoardLocation::Parent {
                            card_id: card.id.clone(),
                        })
                }) {
                    record_move(
                        &mut child,
                        BoardLocation::Lane {
                            project_id: old_board.project_id.clone(),
                            lane_id: old_lane.clone(),
                        },
                        &BoardActor::ProjectChange,
                        None,
                    )?;
                    persist_card(db, &old_board.project_id, &child)?;
                }
                let destination = read(db, project, vec![])?;
                record_move(
                    &mut card,
                    BoardLocation::Lane {
                        project_id: project.clone(),
                        lane_id: destination.lanes[0].id.clone(),
                    },
                    &BoardActor::ProjectChange,
                    None,
                )?;
            }
            persist_card(db, project, &card)?;
        }
    }
    Ok(())
}
