use super::*;
use crate::grading::{RunEvidence, grade};
use crate::world::hard_call;
use clap::Parser as _;

struct Oracle {
    world: World,
    calls: usize,
}
impl Oracle {
    fn call(&mut self, name: &str, args: Value) -> Value {
        self.calls += 1;
        hard_call(&mut self.world, name, &args).unwrap_or_else(|e| panic!("{name}: {e}"))
    }
    fn orders(&mut self, name: &str) -> Vec<Value> {
        let customer = self.call("retail_customer", json!({"name":name}));
        let ids = self.call("retail_orders", json!({"customer_id":customer["id"]}));
        ids.as_array()
            .unwrap()
            .iter()
            .map(|id| self.call("retail_order", json!({"id":id})))
            .collect()
    }
    fn incidents(&mut self, name: &str) -> (Value, Vec<Value>) {
        let service = self.call("ops_service", json!({"name":name}));
        let ids = self.call("ops_tickets", json!({"service_id":service["id"]}));
        let tickets = ids
            .as_array()
            .unwrap()
            .iter()
            .map(|id| self.call("ops_ticket", json!({"id":id})))
            .collect();
        (service, tickets)
    }
}
fn number(value: &Value, key: &str) -> i64 {
    value[key].as_i64().unwrap()
}
fn returnable(order: &Value) -> bool {
    order["status"] == "delivered" && number(order, "age_days") <= 30
}
fn prove(id: &str, solve: impl FnOnce(&mut Oracle) -> Value) {
    let task = hard_pack().into_iter().find(|t| t.id == id).unwrap();
    let mut oracle = Oracle {
        world: task.seed.clone(),
        calls: 0,
    };
    let answer = solve(&mut oracle);
    assert_eq!(oracle.calls, task.tool_calls, "{id} oracle count");
    assert!((4..=10).contains(&oracle.calls));
    let evidence = RunEvidence {
        completed: true,
        finish_value: Some(answer),
        tool_call_count: oracle.calls,
        ..Default::default()
    };
    let result = grade(&task, &oracle.world, &evidence, 0.10);
    assert!(result.passed, "{id}: {:?}", result.failure_reason);
    // Extra writes must fail even when the answer is right.
    oracle.world.kv.insert("unrequested".into(), "write".into());
    assert!(!grade(&task, &oracle.world, &evidence, 0.10).passed);
}

#[test]
fn refund_oracle() {
    prove("hard-retail-refund", |o| {
        let orders = o.orders("Mira");
        let mut total = 0;
        for order in orders.iter().filter(|order| returnable(order)) {
            let receipt = o.call("retail_refund", json!({"order_id":order["id"]}));
            let verified = o.call("retail_order", json!({"id":receipt["order_id"]}));
            total += number(&verified, "refunded_cents");
        }
        json!(total)
    });
}
#[test]
fn exchange_recovery_oracle() {
    prove("hard-retail-exchange", |o| {
        let order = o.call("retail_order", json!({"id":"R7"}));
        let before_refusal = o.world.clone();
        let rejected = o.call(
            "retail_exchange",
            json!({"order_id":order["id"],"sku":"P2"}),
        );
        assert_eq!(rejected, json!({"error":{"code":"out_of_stock"}}));
        assert_eq!(o.world, before_refusal, "refusal must be atomic");
        let original = o.call("retail_product", json!({"sku":order["sku"]}));
        let products = o.call("retail_products", json!({"category":original["category"]}));
        let best = products
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["sku"] != "P2" && number(p, "stock") >= number(&order, "quantity"))
            .min_by_key(|p| number(p, "price_cents"))
            .unwrap();
        let receipt = o.call(
            "retail_exchange",
            json!({"order_id":order["id"],"sku":best["sku"]}),
        );
        o.call("retail_order", json!({"id":receipt["order_id"]}))["sku"].clone()
    });
}
#[test]
fn reschedule_oracle() {
    prove("hard-retail-reschedule", |o| {
        let customer = o.call("retail_customer", json!({"name":"Sana"}));
        if number(&customer, "pending_cents") != 0 {
            return json!("PAYMENT_PENDING");
        }
        let ids = o.call("retail_orders", json!({"customer_id":customer["id"]}));
        let orders = ids
            .as_array()
            .unwrap()
            .iter()
            .map(|id| o.call("retail_order", json!({"id":id})))
            .collect::<Vec<_>>();
        let first = orders
            .iter()
            .filter(|r| r["status"] == "pending")
            .min_by_key(|r| number(r, "delivery_day"))
            .unwrap();
        let receipt = o.call(
            "retail_reschedule",
            json!({"order_id":first["id"],"day":22}),
        );
        o.call("retail_order", json!({"id":receipt["order_id"]}))["delivery_day"].clone()
    });
}
#[test]
fn reprice_oracle() {
    prove("hard-retail-reprice", |o| {
        let orders = o.orders("Mira");
        let total: i64 = orders
            .iter()
            .filter(|r| returnable(r))
            .map(|r| {
                let product = o.call("retail_product", json!({"sku":r["sku"]}));
                number(&product, "price_cents") * number(r, "quantity") - number(r, "paid_cents")
            })
            .sum();
        json!(total)
    });
}
#[test]
fn lamps_oracle() {
    prove("hard-retail-lamps", |o| {
        let orders = o.orders("Mira");
        json!(
            orders
                .iter()
                .filter_map(|r| {
                    let p = o.call("retail_product", json!({"sku":r["sku"]}));
                    (r["status"] == "delivered" && p["category"] == "lamp").then(|| r["id"].clone())
                })
                .collect::<Vec<_>>()
        )
    });
}
#[test]
fn best_return_oracle() {
    prove("hard-retail-best-return", |o| {
        let mut orders = o.orders("Mira");
        orders.retain(returnable);
        orders.sort_by(|a, b| {
            (number(b, "paid_cents") * number(a, "quantity"))
                .cmp(&(number(a, "paid_cents") * number(b, "quantity")))
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        orders[0]["id"].clone()
    });
}
#[test]
fn deploy_recovery_oracle() {
    prove("hard-ops-deploy-recovery", |o| {
        let service = o.call("ops_service", json!({"name":"Beacon"}));
        let ids = o.call("ops_releases", json!({"service_id":service["id"]}));
        let mut releases = ids
            .as_array()
            .unwrap()
            .iter()
            .map(|id| o.call("ops_release", json!({"id":id})))
            .collect::<Vec<_>>();
        releases.retain(|r| r["checks"] == "passed");
        releases.sort_by_key(|r| std::cmp::Reverse(number(r, "version")));
        let before_refusal = o.world.clone();
        let rejected = o.call(
            "ops_deploy",
            json!({"service_id":service["id"],"release_id":releases[0]["id"]}),
        );
        assert_eq!(rejected, json!({"error":{"code":"capacity_exceeded"}}));
        assert_eq!(o.world, before_refusal);
        let fits = releases
            .iter()
            .find(|r| number(r, "required_capacity") <= number(&service, "capacity"))
            .unwrap();
        o.call(
            "ops_deploy",
            json!({"service_id":service["id"],"release_id":fits["id"]}),
        );
        o.call("ops_service", json!({"name":service["name"]}))["deployed_release"].clone()
    });
}
#[test]
fn resolve_chain_oracle() {
    prove("hard-ops-resolve-chain", |o| {
        let incident = o.call("ops_ticket", json!({"id":"I2"}));
        let dependency = o.call("ops_ticket", json!({"id":incident["blocked_by"][0]}));
        let release = o.call("ops_release", json!({"id":dependency["fix_release"]}));
        let service = o.call("ops_service", json!({"name":"Beacon"}));
        assert_eq!(release["checks"], "passed");
        o.call(
            "ops_deploy",
            json!({"service_id":service["id"],"release_id":release["id"]}),
        );
        let receipt = o.call("ops_resolve", json!({"ticket_id":dependency["id"]}));
        assert_eq!(
            o.call("ops_ticket", json!({"id":receipt["ticket_id"]}))["status"],
            "resolved"
        );
        let receipt = o.call("ops_resolve", json!({"ticket_id":incident["id"]}));
        o.call("ops_ticket", json!({"id":receipt["ticket_id"]}))["status"].clone()
    });
}
#[test]
fn impact_oracle() {
    prove("hard-ops-impact", |o| {
        let (_, tickets) = o.incidents("Beacon");
        json!(
            tickets
                .iter()
                .filter(|t| t["status"] == "open" && number(t, "severity") <= 2)
                .map(|t| number(t, "affected_users"))
                .sum::<i64>()
        )
    });
}
#[test]
fn oncall_oracle() {
    prove("hard-ops-oncall", |o| {
        let (beacon, mut tickets) = o.incidents("Beacon");
        let (harbor, more) = o.incidents("Harbor");
        tickets.extend(more);
        tickets.retain(|t| t["status"] == "open");
        tickets.sort_by_key(|t| {
            (
                number(t, "severity"),
                std::cmp::Reverse(number(t, "affected_users")),
            )
        });
        let service = if tickets[0]["service_id"] == beacon["id"] {
            beacon
        } else {
            harbor
        };
        let team = o.call("ops_team", json!({"id":service["team_id"]}));
        team[if team["primary_available"] == true {
            "primary"
        } else {
            "backup"
        }]
        .clone()
    });
}
#[test]
fn ready_oracle() {
    prove("hard-ops-ready", |o| {
        let (service, tickets) = o.incidents("Beacon");
        let mut ready = Vec::new();
        for t in &tickets {
            if t["status"] != "open"
                || t["blocked_by"].as_array().unwrap().iter().any(|id| {
                    tickets
                        .iter()
                        .any(|t| t["id"] == *id && t["status"] == "open")
                })
            {
                continue;
            }
            let r = o.call("ops_release", json!({"id":t["fix_release"]}));
            if r["checks"] == "passed"
                && number(&r, "required_capacity") <= number(&service, "capacity")
            {
                ready.push(t["id"].clone());
            }
        }
        json!(ready)
    });
}
#[test]
fn blocked_impact_oracle() {
    prove("hard-ops-blocked-impact", |o| {
        let first = o.call("ops_ticket", json!({"id":"I2"}));
        let (_, mut tickets) = o.incidents("Harbor");
        tickets.push(first);
        let mut releases = std::collections::BTreeMap::new();
        let mut total = 0;
        for t in tickets.iter().filter(|t| t["status"] == "open") {
            let id = t["fix_release"].as_str().unwrap();
            let r = releases
                .entry(id.to_string())
                .or_insert_with(|| o.call("ops_release", json!({"id":id})));
            if r["checks"] == "failed" {
                total += number(t, "affected_users");
            }
        }
        json!(total)
    });
}
#[test]
fn pack_selection_and_structural_matchers() {
    assert_eq!(hard_pack().len(), 12);
    assert_eq!(crate::selected_tasks(Pack::Hard, &[]).unwrap().len(), 12);
    assert_eq!(crate::selected_tasks(Pack::Easy, &[]).unwrap().len(), 16);
    assert_eq!(crate::selected_tasks(Pack::All, &[]).unwrap().len(), 28);
    assert!(crate::selected_tasks(Pack::Easy, &["hard-ops-impact".into()]).is_err());
    assert!(crate::Args::try_parse_from(["toolbench", "--pack", "invalid"]).is_err());
    let set = FinishMatcher::UnorderedSet(vec![json!({"id":"a"}), json!({"id":"b"})]);
    assert!(set.matches(Some(&json!([{"id":"b"},{"id":"a"}]))));
    assert!(set.matches(Some(&json!([{"id":"a"},{"id":"a"},{"id":"b"}]))));
    for bad in [
        json!([{"id":"a"}]),
        json!([{"id":"a"},{"id":"b"},{"id":"c"}]),
        json!(["a", "b"]),
        json!({"ids":["a","b"]}),
    ] {
        assert!(!set.matches(Some(&bad)));
    }
    let object = FinishMatcher::Exact(json!({"a":1,"b":[2,3]}));
    assert!(object.matches(Some(&json!({"b":[2,3],"a":1}))));
    assert!(!object.matches(Some(&json!({"a":1,"b":[3,2]}))));
    assert!(!object.matches(Some(&json!({"a":1,"b":[2,3],"extra":0}))));
}
#[test]
fn refusals_and_strict_inputs_leave_world_unchanged() {
    let mut world = World::seeded();
    for (name, args, code) in [
        (
            "retail_reschedule",
            json!({"order_id":"R4","day":22}),
            "pending_payment",
        ),
        (
            "retail_reschedule",
            json!({"order_id":"R6","day":22}),
            "not_pending",
        ),
        (
            "retail_refund",
            json!({"order_id":"R2"}),
            "outside_return_policy",
        ),
        (
            "retail_exchange",
            json!({"order_id":"R4","sku":"P4"}),
            "pending_payment",
        ),
        (
            "retail_exchange",
            json!({"order_id":"R7","sku":"P6"}),
            "category_mismatch",
        ),
        (
            "ops_deploy",
            json!({"service_id":"S2","release_id":"V5"}),
            "checks_failed",
        ),
        (
            "ops_deploy",
            json!({"service_id":"S1","release_id":"V4"}),
            "wrong_service",
        ),
        ("ops_resolve", json!({"ticket_id":"I2"}), "dependency_open"),
        ("ops_resolve", json!({"ticket_id":"I1"}), "fix_not_deployed"),
    ] {
        assert_eq!(
            hard_call(&mut world, name, &args).unwrap(),
            json!({"error":{"code":code}})
        );
        assert_eq!(world, World::seeded());
    }
    for args in [
        json!({"id":1}),
        json!({"id":"R1","extra":true}),
        json!({}),
        json!(null),
    ] {
        assert!(hard_call(&mut world, "retail_order", &args).is_err());
        assert_eq!(world, World::seeded());
    }
}

#[test]
fn repeated_refund_and_exchange_do_not_duplicate_mutations() {
    let mut world = World::seeded();
    for (name, args) in [
        ("retail_refund", json!({"order_id":"R1"})),
        ("retail_exchange", json!({"order_id":"R7","sku":"P5"})),
    ] {
        let first = hard_call(&mut world, name, &args).unwrap();
        assert!(first.get("error").is_none());
        let after = world.clone();
        assert_eq!(hard_call(&mut world, name, &args).unwrap(), first);
        assert_eq!(world, after);
    }
}

#[test]
fn integer_arguments_accept_zero_fraction_without_truncation() {
    let mut world = World::seeded();
    hard_call(
        &mut world,
        "retail_reschedule",
        &json!({"order_id":"R7","day":22.0}),
    )
    .unwrap();
    assert_eq!(world.retail.orders[6].delivery_day, 22);
    let before = world.clone();
    for day in [json!(22.5), json!("22"), json!(9223372036854775808_u64)] {
        assert!(
            hard_call(
                &mut world,
                "retail_reschedule",
                &json!({"order_id":"R7","day":day})
            )
            .is_err()
        );
        assert_eq!(world, before);
    }
}
