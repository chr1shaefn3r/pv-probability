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
//!
//! A thermostat on a radiator usually reports its reading with an offset of its own
//! applied, set so that it heats until the far side of the room is warm enough. Such a
//! reading is not comparable to a plain thermometer's, so a room can say which offset to
//! take back out: the `number` entity holding it, or a fixed value.
//!
//! ```toml
//! rooms = [
//!   { name = "Bathroom", sensor = "sensor.bathroom_radiator_temperature",
//!     offset = "number.bathroom_radiator_temperature_offset" },
//! ]
//! ```

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
    /// The offset the thermometer applies to what it measures, if it is to be taken out.
    pub offset: Offset,
}

/// What the rooms file says about a thermometer's offset.
#[derive(Debug, Clone, PartialEq)]
pub enum Offset {
    /// Nothing: the reading is used as reported, and an offset entity that looks like
    /// this thermometer's may be pointed out.
    Unset,
    /// `offset = false`: the reading is used as reported, and nothing is pointed out.
    Off,
    /// Take back out the value this entity - a thermostat's offset setting - held at each
    /// moment.
    Entity { entity: String, applied: Applied },
    /// Take back out the same offset at every moment.
    Fixed { degrees: f64, applied: Applied },
}

impl Offset {
    /// Whether the readings are corrected at all.
    pub fn is_correction(&self) -> bool {
        matches!(self, Offset::Entity { .. } | Offset::Fixed { .. })
    }
}

/// How a thermostat applies its offset to what it measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Applied {
    /// It reports what it measures plus the offset: an offset of -2 reads two degrees
    /// low. This is how Zigbee and Matter define a thermostat's calibration.
    #[default]
    Added,
    /// It reports what it measures minus the offset.
    Subtracted,
}

impl Applied {
    /// What the rooms file calls it.
    pub fn name(self) -> &'static str {
        match self {
            Applied::Added => "added",
            Applied::Subtracted => "subtracted",
        }
    }

    /// The sign the offset was applied with: the reading is what was measured plus this
    /// times the offset.
    pub fn sign(self) -> f64 {
        match self {
            Applied::Added => 1.0,
            Applied::Subtracted => -1.0,
        }
    }
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
    /// An entity id, a number of degrees, or `false`; read as it is to name the room in
    /// any mistake.
    offset: Option<toml::Value>,
    offset_applied: Option<String>,
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
        let mut offsets: HashMap<String, String> = HashMap::new();
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
                let offset = offset(&label, room.offset, room.offset_applied)?;
                if let Offset::Entity { entity, .. } = &offset {
                    if let Some(owner) = offsets.get(entity) {
                        bail!(
                            "{entity} is the offset of both {owner:?} and {label:?}; a \
                             thermostat's offset belongs to its own room"
                        );
                    }
                    offsets.insert(entity.clone(), label.clone());
                }
                owners.insert(sensor.clone(), label);
                rooms.push(Room {
                    name,
                    sensor,
                    offset,
                });
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

/// Check what a room says about its thermometer's offset.
fn offset(label: &str, value: Option<toml::Value>, applied: Option<String>) -> Result<Offset> {
    let applied_given = applied.is_some();
    let applied = match applied.as_deref().map(str::trim) {
        None | Some("added") => Applied::Added,
        Some("subtracted") => Applied::Subtracted,
        Some(other) => bail!(
            "the offset_applied of {label:?} is {other:?}; it is \"added\" (the default, for a \
             thermostat that reports what it measures plus the offset) or \"subtracted\""
        ),
    };
    let offset = match value {
        None => Offset::Unset,
        Some(toml::Value::Boolean(false)) => Offset::Off,
        Some(toml::Value::String(entity)) => {
            let entity = entity.trim().to_string();
            ensure!(
                entity
                    .split_once('.')
                    .is_some_and(|(domain, id)| !domain.is_empty() && !id.is_empty()),
                "the offset of {label:?} is {entity:?}, which is not an entity id; name the \
                 thermostat's offset setting, such as number.bathroom_radiator_temperature_offset"
            );
            Offset::Entity { entity, applied }
        }
        Some(toml::Value::Integer(degrees)) => Offset::Fixed {
            degrees: degrees as f64,
            applied,
        },
        Some(toml::Value::Float(degrees)) => {
            ensure!(
                degrees.is_finite(),
                "the offset of {label:?} is {degrees}, which is not a number of degrees"
            );
            Offset::Fixed { degrees, applied }
        }
        Some(other) => bail!(
            "the offset of {label:?} is {other}; give the entity holding it, as a string, a \
             fixed number of degrees, or false for none"
        ),
    };
    ensure!(
        !applied_given || offset.is_correction(),
        "{label:?} says how its offset is applied, but names no offset to take out"
    );
    Ok(offset)
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

    /// One room on one floor, with `extra` written into its entry.
    fn one_room(extra: &str) -> Result<House> {
        House::parse(&format!(
            "[[floor]]\nname = \"Upstairs\"\nrooms = [{{ name = \"Bath\", \
             sensor = \"sensor.bath_radiator_temperature\"{extra} }}]"
        ))
    }

    fn offset_of(extra: &str) -> Offset {
        one_room(extra).unwrap().floors[0].rooms[0].offset.clone()
    }

    #[test]
    fn a_room_names_the_offset_to_take_back_out() {
        assert_eq!(offset_of(""), Offset::Unset);
        assert_eq!(offset_of(", offset = false"), Offset::Off);
        assert_eq!(
            offset_of(", offset = \" number.bath_temperature_offset \""),
            Offset::Entity {
                entity: "number.bath_temperature_offset".to_string(),
                applied: Applied::Added
            }
        );
        assert_eq!(
            offset_of(", offset = -2"),
            Offset::Fixed {
                degrees: -2.0,
                applied: Applied::Added
            },
            "a whole number of degrees"
        );
        assert_eq!(
            offset_of(", offset = 1.5, offset_applied = \"subtracted\""),
            Offset::Fixed {
                degrees: 1.5,
                applied: Applied::Subtracted
            }
        );
        assert!(offset_of(", offset = 0.5").is_correction());
        assert!(!offset_of(", offset = false").is_correction());
    }

    #[test]
    fn offset_mistakes_name_the_room() {
        let error = |extra: &str| format!("{:#}", one_room(extra).unwrap_err());
        let not_an_id = error(", offset = \"bath offset\"");
        assert!(not_an_id.contains("\"Upstairs / Bath\""), "{not_an_id}");
        assert!(not_an_id.contains("not an entity id"), "{not_an_id}");
        assert!(error(", offset = true").contains("false for none"));
        assert!(error(", offset = nan").contains("not a number of degrees"));
        assert!(error(", offset = 1.0, offset_applied = \"minus\"").contains("\"subtracted\""));
        assert!(
            error(", offset_applied = \"subtracted\"").contains("names no offset"),
            "how it is applied means nothing without an offset"
        );
        assert!(error(", offset = false, offset_applied = \"added\"").contains("names no offset"));

        let twice = House::parse(
            "[[floor]]\nname = \"A\"\nrooms = [\
             { name = \"X\", sensor = \"s.x\", offset = \"number.o\" }, \
             { name = \"Y\", sensor = \"s.y\", offset = \"number.o\" }]",
        )
        .unwrap_err();
        assert!(
            format!("{twice:#}").contains("number.o is the offset of both \"A / X\" and \"A / Y\""),
            "{twice:#}"
        );
    }

    #[test]
    fn a_missing_file_says_which() {
        let error = House::read(Path::new("/nonexistent/rooms.toml")).unwrap_err();
        assert!(format!("{error:#}").contains("/nonexistent/rooms.toml"));
    }
}
