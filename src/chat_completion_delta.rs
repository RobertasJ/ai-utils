#![allow(dead_code)]

use std::collections::HashMap;

use crate::{ChatDelta, ChatRequestBuilder, Choice, FunctionCall, Message};
use futures_util::StreamExt;
use reqwest_eventsource::Event;

use crate::chat_completion_request::serialize;
use crate::{Chat, ChoiceDelta};
use reqwest_eventsource::EventSource;
use serde_derive::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionDelta {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChoiceDelta>,
}

pub struct DeltaReceiver<'a> {
    pub receiver: Receiver<anyhow::Result<ChatCompletionDelta>>,
    pub builder: &'a ChatRequestBuilder,
    pub deltas: Vec<ChatCompletionDelta>,
}

impl<'a> DeltaReceiver<'a> {
    pub fn from(
        receiver: Receiver<anyhow::Result<ChatCompletionDelta>>,
        builder: &'a ChatRequestBuilder,
    ) -> Self {
        Self {
            receiver,
            builder,
            deltas: Vec::new(),
        }
    }
    
    // dont kill me for this type
    pub async fn receive(&mut self, choice_index: i64) -> anyhow::Result<Option<ChatCompletionDelta>> {
        
        loop {
            if let Some(delta) = self.receiver.recv().await {
                let delta = delta?;
                self.deltas.push(delta.clone());
                for choice in &delta.choices {
                    if choice.index == choice_index {
                        return Ok(Some(delta));
                    }
                }
            } else {
                return Ok(None);
            }
        }
    }
    
    pub async fn receive_all(&mut self) -> anyhow::Result<Option<ChatCompletionDelta>> {
        if let Some(delta) = self.receiver.recv().await {
            let delta = delta?;
            self.deltas.push(delta.clone());
            Ok(Some(delta))
        } else {
            Ok(None)
        }
    }

    pub async fn construct_chat(&mut self) -> anyhow::Result<Chat> {
        // make sure you get the full response first
        while let Some(delta) = self.receive_all().await? {
            if delta.choices[0].finish_reason.is_some() {
                break;
            }
        }

        let choice_list: Vec<ChoiceDelta> = self
            .deltas
            .iter()
            .flat_map(|delta| delta.choices.clone())
            .collect();

        let mut choices_map: HashMap<i64, Vec<ChoiceDelta>> = Default::default();
        choice_list.into_iter().for_each(|choice| {
            choices_map.entry(choice.index).or_default().push(choice);
        });

        let choices: Vec<Choice> = choices_map
            .iter()
            .map(|(i, choices)| {
                let index = *i;
                let mut finish_reason: String = Default::default();
                // message part
                let mut role: Option<String> = Option::None;
                let mut content: Option<String> = Option::None;
                let mut function_call = false;
                let mut funtion_call_name: Option<String> = Option::None;
                let mut arguments: Option<String> = Option::None;

                choices.iter().for_each(|choice| {
                    if let Some(reason) = &choice.finish_reason {
                        finish_reason = reason.clone();
                    }

                    if let Some(role_) = &choice.delta.role {
                        role = Some(role_.clone());
                    }

                    if let Some(c) = &choice.delta.content {
                        if let Some(content_) = &mut content {
                            content_.push_str(c);
                        } else {
                            content = Some(c.clone());
                        }
                    }

                    if let Some(call) = &choice.delta.function_call {
                        function_call = true;
                        if let Some(name) = &call.name {
                            funtion_call_name = Some(name.clone());
                        }

                        if let Some(args) = &call.arguments {
                            if let Some(args_) = &mut arguments {
                                args_.push_str(args);
                            } else {
                                arguments = Some(args.clone());
                            }
                        }
                    }
                });

                Choice {
                    index,
                    message: Message {
                        // role should always be there, panic otherwise make this return an error later
                        role: role.unwrap(),
                        content,
                        name: None,
                        function_call: match function_call {
                            true => Some(FunctionCall {
                                name: funtion_call_name.unwrap(),
                                arguments: serde_json::from_str(&arguments.unwrap()).unwrap(),
                            }),
                            false => None,
                        },
                    },
                    finish_reason,
                }
            })
            .collect();

        Ok(Chat {
            id: self.deltas[0].id.clone(),
            object: self.deltas[0].object.clone(),
            created: self.deltas[0].created,
            model: self.deltas[0].model.clone(),
            //will be computed
            choices,
            // approximation
            usage: crate::Usage {
                // unknown
                prompt_tokens: 0,
                completion_tokens: self.deltas.len() as i64,
                total_tokens: self.deltas.len() as i64,
            },
        })
    }
}

pub async fn forward_stream(
    mut es: EventSource,
    tx: Sender<anyhow::Result<ChatDelta>>,
) -> anyhow::Result<()> {
    // Process each event from the EventSource
    while let Some(event) = es.next().await {
        // Handle errors in the event
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                tx.send(Err(err.into())).await?;
                return Ok(());
            }
        };

        // Process Message events
        if let Event::Message(message) = event {
            // Break the loop if the message data is "[DONE]"
            if message.data == "[DONE]" {
                break;
            }

            // Serialize the message data and send it
            let chat: anyhow::Result<ChatDelta> = serialize(&message.data);
            tx.send(chat).await?;
        }
    }

    Ok(())
}
