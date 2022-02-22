use bollard::models::ContainerSummaryInner;

mod config;
mod docker;
mod rfc1035;

pub use config::Config;
pub use docker::DockerListener;
use rfc1035::Class;
pub use rfc1035::{RecordData, ResourceRecord};

pub fn generate_records(
    containers: &Vec<ContainerSummaryInner>,
) -> Result<Vec<ResourceRecord>, Box<dyn std::error::Error>> {
    let mut results = Vec::new();

    for container in containers {
        results.push(ResourceRecord {
            name: String::from("foo"),
            class: Class::In,
            ttl: 300,
            data: RecordData::Cname(String::from("bar")),
        });
    }

    Ok(results)
}
