use crate::*;
use parking_lot::RwLock;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use std::collections::VecDeque;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::thread::{sleep, spawn};
use std::time::Duration;
use tiny_http::{Method, Response, ResponseBox, StatusCode};

// (expectation, name, encoded path to discovery)
type Property = (Expectation, String, Option<String>);

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct StatusView {
    done: bool,
    model: String,
    state_count: usize,
    unique_state_count: usize,
    max_depth: usize,
    properties: Vec<Property>,
    recent_path: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct StateView<State> {
    action: Option<String>,
    outcome: Option<String>,
    state: Option<State>,
    properties: Vec<Property>,
    svg: Option<String>,
}

impl<State> serde::Serialize for StateView<State>
where
    State: Debug + Hash,
{
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut out = ser.serialize_struct("StateView", 3)?;
        if let Some(ref action) = self.action {
            out.serialize_field("action", action)?;
        }
        if let Some(ref outcome) = self.outcome {
            out.serialize_field("outcome", outcome)?;
        }
        if let Some(ref state) = self.state {
            out.serialize_field("state", &format!("{:#?}", state))?;
            out.serialize_field("fingerprint", &format!("{:?}", fingerprint(&state)))?;
        }
        if !self.properties.is_empty() {
            out.serialize_field("properties", &self.properties)?;
        }
        if let Some(ref svg) = self.svg {
            out.serialize_field("svg", svg)?;
        }
        out.end()
    }
}

struct Snapshot<Action>(bool, Option<Vec<Action>>);
impl<M: Model> CheckerVisitor<M> for Arc<RwLock<Snapshot<M::Action>>> {
    fn visit(&self, _: &M, path: Path<M::State, M::Action>) {
        let guard = self.read();
        if !guard.0 {
            return;
        }
        drop(guard);

        let mut guard = self.write();
        if !guard.0 {
            return;
        } // May be racing other threads.
        guard.0 = false;
        guard.1 = Some(path.into_actions());
    }
}

pub(crate) fn serve<M>(
    checker_builder: CheckerBuilder<M>,
    addresses: impl ToSocketAddrs,
) -> Arc<impl Checker<M>>
where
    M: 'static + Model + Send + Sync,
    M::Action: Debug + Send + Sync,
    M::State: Debug + Hash + Send + Sync,
{
    let snapshot = Arc::new(RwLock::new(Snapshot(true, None)));
    let snapshot_for_visitor = Arc::clone(&snapshot);
    let snapshot_for_server = Arc::clone(&snapshot);
    spawn(move || loop {
        sleep(Duration::from_secs(4));
        snapshot.write().0 = true;
    });
    let checker = checker_builder
        .visitor(snapshot_for_visitor)
        .spawn_on_demand();
    serve_checker(checker, snapshot_for_server, addresses)
}

fn serve_checker<M, C>(
    checker: C,
    snapshot: Arc<RwLock<Snapshot<M::Action>>>,
    addresses: impl ToSocketAddrs,
) -> Arc<impl Checker<M>>
where
    M: 'static + Model + Send + Sync,
    M::Action: Debug + Send + Sync,
    M::State: Debug + Hash + Send + Sync,
    C: 'static + Checker<M> + Send + Sync,
{
    let checker = Arc::new(checker);

    let server = tiny_http::Server::http(addresses).unwrap();

    let server = Arc::new(server);

    macro_rules! get_ui_file {
        ($filename:literal) => {{
            let data = if let Ok(content) = std::fs::read(concat!("./ui/", $filename)) {
                log::info!("Explorer dev mode. Loading {} from disk.", $filename);
                content
            } else {
                log::info!("Explorer release mode. Loading {} from disk.", $filename);
                include_bytes!(concat!("../../ui/", $filename)).to_vec()
            };
            Response::from_data(data).boxed()
        }};
    }

    let data = Arc::new((snapshot, Arc::clone(&checker)));
    let web_handle = std::thread::spawn(move || loop {
        let rq = server.recv().unwrap();
        let response = match (rq.method(), rq.url()) {
            (Method::Get, "/") => get_ui_file!("index.htm"),
            (Method::Get, "/app.css") => get_ui_file!("app.css"),
            (Method::Get, "/app.js") => get_ui_file!("app.js"),
            (Method::Get, "/knockout-3.5.0.js") => get_ui_file!("knockout-3.5.0.js"),
            (Method::Get, "/.status") => {
                let view = status(Arc::clone(&data));
                let status_json = serde_json::to_vec(&view).unwrap();
                Response::from_data(status_json).boxed()
            }
            (Method::Post, "/.runtocompletion") => run_to_completion(Arc::clone(&data)),
            (Method::Get, url) => {
                if let Some(fingerprints) = url.strip_prefix("/.states") {
                    match states(fingerprints, Arc::clone(&data)) {
                        Ok(states) => {
                            let states_json = serde_json::to_vec(&states).unwrap();
                            Response::from_data(states_json).boxed()
                        }
                        Err(err) => Response::from_string(err)
                            .with_status_code(StatusCode(404))
                            .boxed(),
                    }
                } else {
                    Response::empty(StatusCode(404)).boxed()
                }
            }
            _ => Response::empty(StatusCode(404)).boxed(),
        };
        rq.respond(response).unwrap();
    });
    web_handle.join().unwrap();

    checker
}

type Data<Action, Checker> = Arc<(Arc<RwLock<Snapshot<Action>>>, Arc<Checker>)>;

fn status<M, C>(data: Data<M::Action, C>) -> StatusView
where
    M: Model,
    M::Action: Debug,
    M::State: Hash,
    C: Checker<M>,
{
    let snapshot = &data.0;
    let checker = &data.1;

    StatusView {
        model: std::any::type_name::<M>().to_string(),
        done: checker.is_done(),
        state_count: checker.state_count(),
        unique_state_count: checker.unique_state_count(),
        max_depth: checker.max_depth(),
        properties: get_properties(checker),
        recent_path: snapshot.read().1.as_ref().map(|p| format!("{:?}", p)),
    }
}

fn run_to_completion<M, C>(data: Data<M::Action, C>) -> ResponseBox
where
    M: Model,
    M::Action: Debug,
    M::State: Hash,
    C: Checker<M>,
{
    let checker = &data.1;
    checker.run_to_completion();
    Response::empty(StatusCode(200)).boxed()
}

fn get_properties<C, M>(checker: &Arc<C>) -> Vec<Property>
where
    M: Model,
    M::State: Hash,
    C: Checker<M>,
{
    checker
        .model()
        .properties()
        .into_iter()
        .map(|p| {
            (
                p.expectation,
                p.name.to_string(),
                checker.discovery(p.name).map(|p| p.encode()),
            )
        })
        .collect()
}

fn states<M, C>(path: &str, data: Data<M::Action, C>) -> Result<Vec<StateView<M::State>>, String>
where
    M: Model,
    M::Action: Debug,
    M::State: Debug + Hash,
    C: Checker<M>,
{
    let checker = &data.1;
    let model = &checker.model();

    // extract fingerprints
    let mut fingerprints_str = path.to_string();
    if fingerprints_str.ends_with('/') {
        let relevant_len = fingerprints_str.len() - 1;
        fingerprints_str.truncate(relevant_len);
    }
    let fingerprints: VecDeque<_> = fingerprints_str
        .split('/')
        .filter_map(|fp| fp.parse::<Fingerprint>().ok())
        .collect();

    // ensure all but the first string (which is empty) were parsed
    if fingerprints.len() + 1 != fingerprints_str.split('/').count() {
        return Err(format!("Unable to parse fingerprints {}", fingerprints_str));
    }

    // now build up all the subsequent `StateView`s
    let mut results = Vec::new();
    if fingerprints.is_empty() {
        for state in model.init_states() {
            let fingerprint = fingerprint(&state);
            checker.check_fingerprint(fingerprint);
            let svg = {
                let mut fingerprints: VecDeque<_> = fingerprints.clone().into_iter().collect();
                fingerprints.push_back(fingerprint);
                model.as_svg(Path::from_fingerprints::<M>(model, fingerprints))
            };
            results.push(StateView {
                action: None,
                outcome: None,
                state: Some(state),
                properties: get_properties(checker),
                svg,
            });
        }
    } else if let Some(last_state) = Path::final_state::<M>(model, fingerprints.clone()) {
        // Must generate the actions three times because they are consumed by `next_state`
        // and `display_outcome`.
        let mut actions1 = Vec::new();
        let mut actions2 = Vec::new();
        let mut actions3 = Vec::new();
        model.actions(&last_state, &mut actions1);
        model.actions(&last_state, &mut actions2);
        model.actions(&last_state, &mut actions3);
        for ((action, action2), action3) in actions1.into_iter().zip(actions2).zip(actions3) {
            let outcome = model.format_step(&last_state, action2);
            let state = model.next_state(&last_state, action3);
            log::debug!(
                "explorer generated state transition: {} -> {}",
                fingerprint(&last_state),
                fingerprint(&state)
            );
            if let Some(state) = state {
                let fingerprint = fingerprint(&state);
                checker.check_fingerprint(fingerprint);
                let svg = {
                    let mut fingerprints: VecDeque<_> = fingerprints.clone().into_iter().collect();
                    fingerprints.push_back(fingerprint);
                    model.as_svg(Path::from_fingerprints::<M>(model, fingerprints))
                };
                results.push(StateView {
                    action: Some(model.format_action(&action)),
                    outcome,
                    state: Some(state),
                    properties: get_properties(checker),
                    svg,
                });
            } else {
                // "Action ignored" case is still returned, as it may be useful for debugging.
                results.push(StateView {
                    action: Some(model.format_action(&action)),
                    outcome: None,
                    state: None,
                    properties: get_properties(checker),
                    svg: None,
                });
            }
        }
    } else {
        return Err(format!(
            "Unable to find state following fingerprints {}",
            fingerprints_str
        ));
    }

    Ok(results)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::actor::{RandomChoices, Timers};
    use crate::test_util::binary_clock::*;
    use lazy_static::lazy_static;

    #[test]
    fn can_init() {
        let checker = Arc::new(BinaryClock.checker().spawn_bfs().join());
        assert_eq!(
            get_states(Arc::clone(&checker), "/").unwrap(),
            vec![
                StateView {
                    action: None,
                    outcome: None,
                    state: Some(0),
                    properties: vec![(Expectation::Always, "in [0, 1]".to_owned(), None)],
                    svg: None
                },
                StateView {
                    action: None,
                    outcome: None,
                    state: Some(1),
                    properties: vec![(Expectation::Always, "in [0, 1]".to_owned(), None)],
                    svg: None
                },
            ]
        );
    }

    #[test]
    fn can_next() {
        let checker = Arc::new(BinaryClock.checker().spawn_bfs().join());
        // We need a static string for TestRequest, so this is precomputed, but you can recompute
        // the values if needed as follows:
        // ```
        // let first = fingerprint(&1_i8);
        // let second = fingerprint(&0_i8);
        // let path_name = format!("/{}/{}", first, second);
        // println!("New path name is: {}", path_name);
        // ```
        let first = fingerprint(&1_i8);
        let second = fingerprint(&0_i8);
        println!("Expecting path: /{}/{}", first, second);
        assert_eq!(
            get_states(
                Arc::clone(&checker),
                "/9393718671459482478/5869721577187787215"
            )
            .unwrap(),
            vec![StateView {
                action: Some("GoHigh".to_string()),
                outcome: Some("1".to_string()),
                state: Some(1),
                properties: vec![(Expectation::Always, "in [0, 1]".to_owned(), None)],
                svg: None,
            },]
        );
    }

    #[test]
    fn err_for_invalid_fingerprint() {
        let checker = Arc::new(BinaryClock.checker().spawn_bfs().join());
        assert_eq!(
            format!(
                "{}",
                get_states(Arc::clone(&checker), "/one/two/three").unwrap_err()
            ),
            "Unable to parse fingerprints /one/two/three"
        );
        assert_eq!(
            format!(
                "{}",
                get_states(Arc::clone(&checker), "/1/2/3").unwrap_err()
            ),
            "Unable to find state following fingerprints /1/2/3"
        );
    }

    #[test]
    fn smoke_test_states() {
        use crate::actor::actor_test_util::ping_pong::{PingPongCfg, PingPongMsg::*};
        use crate::actor::{ActorModelState, Envelope, Id, LossyNetwork, Network};

        let checker = Arc::new(
            PingPongCfg {
                max_nat: 2,
                maintains_history: true,
            }
            .into_model()
            .init_network(Network::new_unordered_nonduplicating([]))
            .lossy_network(LossyNetwork::Yes)
            .checker()
            .spawn_bfs()
            .join(),
        );
        assert_eq!(
            get_states(Arc::clone(&checker), "/").unwrap(),
            vec![
                StateView {
                    action: None,
                    outcome: None,
                    state: Some(ActorModelState {
                        actor_states: vec![Arc::new(0), Arc::new(0)],
                        history: (0, 1),
                        timers_set: vec![Timers::new(); 2],
                        random_choices: vec![RandomChoices::default(); 2],
                        crashed: vec![false; 2],
                        network: Network::new_unordered_nonduplicating([
                            Envelope { src: Id::from(0), dst: Id::from(1), msg: Ping(0) },
                        ]),
                        actor_storages: vec![None; 2],
                    }),
                    properties: vec![
                        (Expectation::Always, "delta within 1".into(), None),
                        (Expectation::Sometimes, "can reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315".into())),
                        (Expectation::Eventually, "must reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                        (Expectation::Eventually, "must exceed max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                        (Expectation::Always, "#in <= #out".into(), None),
                        (Expectation::Eventually, "#out <= #in + 1".into(), None),
                    ],
                    svg: Some("<svg version=\'1.1\' baseProfile=\'full\' width=\'500\' height=\'30\' viewbox=\'-20 -20 520 50\' xmlns=\'http://www.w3.org/2000/svg\'><defs><marker class=\'svg-event-shape\' id=\'arrow\' markerWidth=\'12\' markerHeight=\'10\' refX=\'12\' refY=\'5\' orient=\'auto\'><polygon points=\'0 0, 12 5, 0 10\' /></marker></defs><line x1=\'0\' y1=\'0\' x2=\'0\' y2=\'30\' class=\'svg-actor-timeline\' />\n<text x=\'0\' y=\'0\' class=\'svg-actor-label\'>0</text>\n<line x1=\'100\' y1=\'0\' x2=\'100\' y2=\'30\' class=\'svg-actor-timeline\' />\n<text x=\'100\' y=\'0\' class=\'svg-actor-label\'>1</text>\n</svg>\n".to_string()),
                },
            ]);

        lazy_static! {
            static ref PATH: String = {
                use crate::actor::actor_test_util::ping_pong::{PingPongActor, PingPongHistory};
                let fp = fingerprint(&ActorModelState::<PingPongActor, PingPongHistory> {
                    actor_states: vec![Arc::new(0), Arc::new(0)],
                    history: (0, 1),
                    timers_set: vec![Timers::new(); 2],
                    random_choices: vec![RandomChoices::default(); 2],
                    crashed: vec![false; 2],
                    network: Network::new_unordered_nonduplicating([Envelope {
                        src: Id::from(0),
                        dst: Id::from(1),
                        msg: Ping(0),
                    }]),
                    actor_storages: vec![None; 2],
                });
                format!("/{}", fp)
            };
        }
        let states = get_states(Arc::clone(&checker), PATH.as_ref()).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(
            states[0],
            StateView {
                action: Some("Drop(Envelope { src: Id(0), dst: Id(1), msg: Ping(0) })".to_string()),
                outcome: Some("DROP: Envelope { src: Id(0), dst: Id(1), msg: Ping(0) }".to_string()),
                state: Some(ActorModelState {
                    actor_states: vec![Arc::new(0), Arc::new(0)],
                    history: (0, 1),
                    timers_set: vec![Timers::new(); 2],
                    random_choices: vec![RandomChoices::default(); 2],
                    crashed: vec![false; 2],
                    network: Network::new_unordered_nonduplicating([]),
                    actor_storages: vec![None; 2],
                }),
                properties: vec![
                    (Expectation::Always, "delta within 1".into(), None),
                    (Expectation::Sometimes, "can reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315".into())),
                    (Expectation::Eventually, "must reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                    (Expectation::Eventually, "must exceed max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                    (Expectation::Always, "#in <= #out".into(), None),
                    (Expectation::Eventually, "#out <= #in + 1".into(), None),
                ],
                svg: Some("<svg version='1.1' baseProfile='full' width='500' height='60' viewbox='-20 -20 520 80' xmlns='http://www.w3.org/2000/svg'><defs><marker class='svg-event-shape' id='arrow' markerWidth='12' markerHeight='10' refX='12' refY='5' orient='auto'><polygon points='0 0, 12 5, 0 10' /></marker></defs><line x1='0' y1='0' x2='0' y2='60' class='svg-actor-timeline' />\n<text x='0' y='0' class='svg-actor-label'>0</text>\n<line x1='100' y1='0' x2='100' y2='60' class='svg-actor-timeline' />\n<text x='100' y='0' class='svg-actor-label'>1</text>\n</svg>\n".to_string()),
            });
        assert_eq!(
            states[1],
            StateView {
                action: Some("Id(0) → Ping(0) → Id(1)".to_string()),
                outcome: Some("OUT: [Send(Id(0), Pong(0))]\n\nNEXT_STATE: 1\n\nPREV_STATE: 0\n".to_string()),
                state: Some(ActorModelState {
                    actor_states: vec![
                        Arc::new(0),
                        Arc::new(1),
                    ],
                    history: (1, 2),
                    timers_set: vec![Timers::new(); 2],
                    random_choices: vec![RandomChoices::default(); 2],
                    crashed: vec![false; 2],
                    network: Network::new_unordered_nonduplicating([
                        Envelope { src: Id::from(1), dst: Id::from(0), msg: Pong(0) },
                    ]),
                    actor_storages: vec![None; 2],
                }),
                properties: vec![
                    (Expectation::Always, "delta within 1".into(), None),
                    (Expectation::Sometimes, "can reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315".into())),
                    (Expectation::Eventually, "must reach max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                    (Expectation::Eventually, "must exceed max".into(), Some("9825351251631602636/3760012235735042049/7133060688412568841/11622042860899162315/5132103924661761264/12325952466011360495".into())),
                    (Expectation::Always, "#in <= #out".into(), None),
                    (Expectation::Eventually, "#out <= #in + 1".into(), None),
                ],
                svg: Some("<svg version='1.1' baseProfile='full' width='500' height='60' viewbox='-20 -20 520 80' xmlns='http://www.w3.org/2000/svg'><defs><marker class='svg-event-shape' id='arrow' markerWidth='12' markerHeight='10' refX='12' refY='5' orient='auto'><polygon points='0 0, 12 5, 0 10' /></marker></defs><line x1='0' y1='0' x2='0' y2='60' class='svg-actor-timeline' />\n<text x='0' y='0' class='svg-actor-label'>0</text>\n<line x1='100' y1='0' x2='100' y2='60' class='svg-actor-timeline' />\n<text x='100' y='0' class='svg-actor-label'>1</text>\n<line x1='0' x2='100' y1='0' y2='30' marker-end='url(#arrow)' class='svg-event-line' />\n<text x='100' y='30' class='svg-event-label'>Ping(0)</text>\n</svg>\n".to_string()),
            });
    }

    #[test]
    fn smoke_test_status() {
        use crate::actor::actor_test_util::ping_pong::PingPongCfg;
        use crate::actor::{LossyNetwork, Network};

        let snapshot = Arc::new(RwLock::new(Snapshot(true, None)));
        let checker = PingPongCfg {
            max_nat: 2,
            maintains_history: true,
        }
        .into_model()
        .init_network(Network::new_unordered_nonduplicating([]))
        .lossy_network(LossyNetwork::No)
        .checker()
        .visitor(Arc::clone(&snapshot))
        .spawn_bfs()
        .join();
        let status = get_status(Arc::new(checker), snapshot);
        assert!(status.done);
        assert_eq!(
            status.model,
            "stateright::actor::model::ActorModel<\
                 stateright::actor::actor_test_util::ping_pong::PingPongActor, \
                 stateright::actor::actor_test_util::ping_pong::PingPongCfg, (u32, u32)>"
        );
        assert_eq!(status.state_count, 5);
        assert_eq!(status.unique_state_count, 5);
        assert_eq!(status.max_depth, 5);
        let assert_discovery = |status: &StatusView,
                                expectation: Expectation,
                                name: &'static str,
                                has_discovery: bool| {
            let match_found = status
                .properties
                .iter()
                .any(|(e, n, d)| e == &expectation && n == name && d.is_some() == has_discovery);
            if !match_found {
                panic!(
                    "Not found. expectation={:?}, name={:?}, has_discovery={:?}, properties={:#?}",
                    expectation, name, has_discovery, status.properties
                );
            }
        };
        assert_discovery(&status, Expectation::Always, "delta within 1", false);
        assert_discovery(&status, Expectation::Sometimes, "can reach max", true);
        assert_discovery(&status, Expectation::Eventually, "must reach max", false);
        assert_discovery(&status, Expectation::Eventually, "must exceed max", true);
        assert_discovery(&status, Expectation::Always, "#in <= #out", false);
        assert_discovery(&status, Expectation::Eventually, "#out <= #in + 1", false);
        assert!(status.recent_path.unwrap().starts_with('['));
    }

    fn get_states<M, C>(
        checker: Arc<C>,
        path_name: &'static str,
    ) -> Result<Vec<StateView<M::State>>, String>
    where
        M: Model,
        M::Action: Debug,
        M::State: Debug + Hash,
        C: Checker<M>,
    {
        let snapshot = Arc::new(RwLock::new(Snapshot(true, None)));
        let data = Arc::new((snapshot, checker));
        states(path_name, data)
    }

    fn get_status<M, C>(checker: Arc<C>, snapshot: Arc<RwLock<Snapshot<M::Action>>>) -> StatusView
    where
        M: Model,
        M::Action: Debug,
        M::State: Debug + Hash,
        C: Checker<M>,
    {
        let data = Arc::new((snapshot, checker));
        status(data)
    }
}
