use serde::{Serialize, Deserialize};
use crate::Snowflake;
use crate::interaction::ApplicationCommandInteractionDataOption;

#[derive(Serialize, Deserialize, Debug)]
pub struct ApplicationCommandInteractionData {
    pub id: Snowflake,
    pub name: Box<str>,
    pub options: Vec<ApplicationCommandInteractionDataOption>,
}
