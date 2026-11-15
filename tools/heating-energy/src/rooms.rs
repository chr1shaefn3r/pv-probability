//! The house: its floors, the rooms on each, and the thermometers in every room.
//!
//! Far too many pairs to type as command line arguments, so they live in a TOML file:
//!
//! ```toml
//! [[floor]]
//! name = "Ground floor"
//! rooms = [
//!   { name = "Living room", sensor = "sensor.living_room_temperature" },
//!   { name = "Kitchen", sensor = "sensor.kitchen_temperature" },
//! ]
//!
//! [[floor]]
//! name = "First floor"
//! rooms = [
//!   { name = "Bedroom", sensor = "sensor.bedroom_temperature" },
//! ]
//! ```
//!
//! A room with more than one thermometer lists them all, and is counted by their mean:
//!
//! ```toml
//! rooms = [
//!   { name = "Living room", sensors = ["sensor.sofa_temperature", "sensor.desk_temperature"] },
//! ]
//! ```
//!
//! Floors and rooms keep the order the file gives them, which is the order the report
//! shows them in.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

/// Every floor of the house, top to bottom or bottom to top - whichever the file chose.
#[derive(Debug, Clone, PartialEq)]
pub struct House {
    pub floors: Vec<Floor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Floor {
    pub name: String,
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Room {
    pub name: String,
    /// The room's thermometers, as Home Assistant entity or statistic ids: one or more,
    /// in the file's order.
    pub sensors: Vec<String>,
}

/// The file as written, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    floor: Vec<FloorEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorEntry {
    name: String,
    #[serde(default)]
    rooms: Vec<RoomEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomEntry {
    name: String,
    /// The one thermometer of a room that has one...
    sensor: Option<String>,
    /// ... or every thermometer of a room that has several.
    sensors: Option<Vec<String>>,
}

impl House {
    /// Read and check a rooms file.
    pub fn read(path: &Path) -> Result<House> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read the rooms file {}", path.display()))?;
        House::parse(&text).with_context(|| format!("in the rooms file {}", path.display()))
    }

    /// Parse and check the contents of a rooms file.
    pub fn parse(text: &str) -> Result<House> {
        let file: File = toml::from_str(text).context("not a valid rooms file")?;
        ensure!(
            !file.floor.is_empty(),
            "it names no floors; start one with [[floor]] and give it a name and its rooms"
        );

        let mut floors = Vec::with_capacity(file.floor.len());
        let mut owners: HashMap<String, String> = HashMap::new();
        for entry in file.floor {
            let floor_name = entry.name.trim().to_string();
            ensure!(!floor_name.is_empty(), "a floor has an empty name");
            ensure!(
                !floors.iter().any(|floor: &Floor| floor.name == floor_name),
                "the floor {floor_name:?} appears twice"
            );
            ensure!(
                !entry.rooms.is_empty(),
                "the floor {floor_name:?} has no rooms; list them as rooms = [{{ name = ..., \
                 sensor = ... }}]"
            );

            let mut rooms: Vec<Room> = Vec::with_capacity(entry.rooms.len());
            for room in entry.rooms {
                let name = room.name.trim().to_string();
                let label = format!("{floor_name} / {name}");
                ensure!(
                    !name.is_empty(),
                    "a room on {floor_name:?} has an empty name"
                );
                ensure!(
                    !rooms.iter().any(|other| other.name == name),
                    "{floor_name:?} has two rooms called {name:?}"
                );
                let sensors = match (room.sensor, room.sensors) {
                    (Some(sensor), None) => vec![sensor],
                    (None, Some(sensors)) => {
                        ensure!(
                            !sensors.is_empty(),
                            "the room {label:?} lists no sensors; give it at least one"
                        );
                        sensors
                    }
                    (None, None) => bail!(
                        "the room {label:?} has no sensor; give it one with sensor = ..., or \
                         several with sensors = [...]"
                    ),
                    (Some(_), Some(_)) => bail!(
                        "the room {label:?} has both sensor and sensors; list all its \
                         thermometers under sensors"
                    ),
                };
                let mut own: Vec<String> = Vec::with_capacity(sensors.len());
                for sensor in sensors {
                    let sensor = sensor.trim().to_string();
                    ensure!(!sensor.is_empty(), "the room {label:?} has an empty sensor");
                    ensure!(
                        !own.contains(&sensor),
                        "{sensor} is listed twice for {label:?}"
                    );
                    if let Some(owner) = owners.get(&sensor) {
                        bail!(
                            "{sensor} is the thermometer of both {owner:?} and {label:?}; list \
                             an open-plan space once, as one room"
                        );
                    }
                    owners.insert(sensor.clone(), label.clone());
                    own.push(sensor);
                }
                rooms.push(Room { name, sensors: own });
            }
            floors.push(Floor {
                name: floor_name,
                rooms,
            });
        }
        Ok(House { floors })
    }

    /// Every room with its floor, in the file's order.
    pub fn rooms(&self) -> impl Iterator<Item = (&Floor, &Room)> {
        self.floors
            .iter()
            .flat_map(|floor| floor.rooms.iter().map(move |room| (floor, room)))
    }

    pub fn room_count(&self) -> usize {
        self.floors.iter().map(|floor| floor.rooms.len()).sum()
    }

    /// Every thermometer of every room, with its room, in the file's order.
    pub fn sensors(&self) -> impl Iterator<Item = (&Floor, &Room, &str)> {
        self.rooms().flat_map(|(floor, room)| {
            room.sensors
                .iter()
                .map(move |sensor| (floor, room, sensor.as_str()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        [[floor]]
        name = "Ground floor"
        rooms = [
          { name = "Living room", sensor = "sensor.living_room_temperature" },
          { name = "Kitchen", sensor = "sensor.kitchen_temperature" },
        ]

        [[floor]]
        name = "First floor"
        rooms = [
          { name = "Bedroom", sensor = "sensor.bedroom_temperature" },
        ]
    "#;

    fn error(text: &str) -> String {
        format!("{:#}", House::parse(text).unwrap_err())
    }

    #[test]
    fn reads_floors_and_rooms_in_the_files_order() {
        let house = House::parse(EXAMPLE).unwrap();
        assert_eq!(house.floors.len(), 2);
        assert_eq!(house.floors[0].name, "Ground floor");
        assert_eq!(house.room_count(), 3);
        let rooms: Vec<(&str, &str, &str)> = house
            .sensors()
            .map(|(floor, room, sensor)| (floor.name.as_str(), room.name.as_str(), sensor))
            .collect();
        assert_eq!(
            rooms,
            [
                (
                    "Ground floor",
                    "Living room",
                    "sensor.living_room_temperature"
                ),
                ("Ground floor", "Kitchen", "sensor.kitchen_temperature"),
                ("First floor", "Bedroom", "sensor.bedroom_temperature"),
            ]
        );
    }

    #[test]
    fn the_example_file_matches_the_demo_database() {
        let house = House::parse(include_str!("../rooms.example.toml")).unwrap();
        let sensors: Vec<&str> = house.sensors().map(|(_, _, sensor)| sensor).collect();
        let demo: Vec<&str> = ha_testdb::ROOMS
            .iter()
            .map(|(_, _, sensor, _)| *sensor)
            .collect();
        assert_eq!(sensors, demo);
    }

    #[test]
    fn the_same_room_name_may_appear_on_two_floors() {
        let house = House::parse(
            r#"
            [[floor]]
            name = "Ground floor"
            rooms = [{ name = "Bathroom", sensor = "sensor.a" }]
            [[floor]]
            name = "First floor"
            rooms = [{ name = "Bathroom", sensor = "sensor.b" }]
            "#,
        )
        .unwrap();
        assert_eq!(house.room_count(), 2);
    }

    #[test]
    fn mistakes_are_named() {
        assert!(error("").contains("names no floors"));
        assert!(error("[[floor]]\nname = \"Attic\"").contains("\"Attic\" has no rooms"));
        assert!(
            error("[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\" }]").contains("sensor"),
            "a room without a sensor: {}",
            error("[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\" }]")
        );
        assert!(
            error(
                "[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\", sensor = \"s.x\", size = 3 }]"
            )
            .contains("size"),
            "unknown keys are refused rather than ignored"
        );
        assert!(
            error(
                "[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\", sensor = \"s.x\" }, \
                 { name = \"X\", sensor = \"s.y\" }]"
            )
            .contains("two rooms called \"X\"")
        );
        assert!(
            error(
                "[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\", sensor = \"s.x\" }]\n\
                 [[floor]]\nname = \"A\"\nrooms = [{ name = \"Y\", sensor = \"s.y\" }]"
            )
            .contains("\"A\" appears twice")
        );
        let shared = error(
            "[[floor]]\nname = \"A\"\nrooms = [{ name = \"X\", sensor = \"s.x\" }, \
             { name = \"Y\", sensor = \"s.x\" }]",
        );
        assert!(
            shared.contains("s.x is the thermometer of both \"A / X\" and \"A / Y\""),
            "{shared}"
        );
        assert!(error("[[floor]]\nname = \"  \"\nrooms = []").contains("empty name"));
    }

    #[test]
    fn a_room_may_have_several_thermometers() {
        let house = House::parse(
            r#"
            [[floor]]
            name = "Ground floor"
            rooms = [
              { name = "Living room", sensors = [" sensor.sofa ", "sensor.desk"] },
              { name = "Kitchen", sensor = "sensor.kitchen" },
            ]
            "#,
        )
        .unwrap();
        assert_eq!(house.room_count(), 2);
        assert_eq!(
            house.floors[0].rooms[0].sensors,
            ["sensor.sofa", "sensor.desk"]
        );
        assert_eq!(house.floors[0].rooms[1].sensors, ["sensor.kitchen"]);
        let all: Vec<(&str, &str)> = house
            .sensors()
            .map(|(_, room, sensor)| (room.name.as_str(), sensor))
            .collect();
        assert_eq!(
            all,
            [
                ("Living room", "sensor.sofa"),
                ("Living room", "sensor.desk"),
                ("Kitchen", "sensor.kitchen")
            ]
        );
    }

    #[test]
    fn several_thermometers_are_checked_like_one() {
        let room = |entry: &str| {
            format!(
                "{:#}",
                House::parse(&format!(
                    "[[floor]]\nname = \"A\"\nrooms = [{{ name = \"X\"{entry} }}, \
                     {{ name = \"Y\", sensor = \"s.y\" }}]"
                ))
                .unwrap_err()
            )
        };
        assert!(room(", sensors = []").contains("lists no sensors"));
        assert!(
            room(", sensor = \"s.x\", sensors = [\"s.z\"]").contains("both sensor and sensors")
        );
        assert!(room("").contains("has no sensor"));
        assert!(
            room(", sensors = [\"s.x\", \" s.x\"]").contains("s.x is listed twice for \"A / X\"")
        );
        assert!(room(", sensors = [\"s.x\", \"\"]").contains("empty sensor"));
        assert!(
            room(", sensors = [\"s.x\", \"s.y\"]")
                .contains("s.y is the thermometer of both \"A / X\" and \"A / Y\""),
            "{}",
            room(", sensors = [\"s.x\", \"s.y\"]")
        );
    }

    #[test]
    fn a_missing_file_says_which() {
        let error = House::read(Path::new("/nonexistent/rooms.toml")).unwrap_err();
        assert!(format!("{error:#}").contains("/nonexistent/rooms.toml"));
    }
}
