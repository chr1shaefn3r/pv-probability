//! The house: its floors, the rooms on each, and the thermometer in every room.
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
    /// The room's thermometer, as a Home Assistant entity or statistic id.
    pub sensor: String,
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
    sensor: String,
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
                let sensor = room.sensor.trim().to_string();
                let label = format!("{floor_name} / {name}");
                ensure!(
                    !name.is_empty(),
                    "a room on {floor_name:?} has an empty name"
                );
                ensure!(
                    !rooms.iter().any(|other| other.name == name),
                    "{floor_name:?} has two rooms called {name:?}"
                );
                ensure!(!sensor.is_empty(), "the room {label:?} has an empty sensor");
                if let Some(owner) = owners.get(&sensor) {
                    bail!(
                        "{sensor} is the thermometer of both {owner:?} and {label:?}; list an \
                         open-plan space once, as one room"
                    );
                }
                owners.insert(sensor.clone(), label);
                rooms.push(Room { name, sensor });
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
            .rooms()
            .map(|(floor, room)| {
                (
                    floor.name.as_str(),
                    room.name.as_str(),
                    room.sensor.as_str(),
                )
            })
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
        let sensors: Vec<&str> = house
            .rooms()
            .map(|(_, room)| room.sensor.as_str())
            .collect();
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
    fn a_missing_file_says_which() {
        let error = House::read(Path::new("/nonexistent/rooms.toml")).unwrap_err();
        assert!(format!("{error:#}").contains("/nonexistent/rooms.toml"));
    }
}
