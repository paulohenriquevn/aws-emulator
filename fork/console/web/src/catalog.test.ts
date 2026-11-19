import { describe, expect, it } from "vitest";
import { findResource, findService, SERVICES, servicesByCategory } from "./catalog";

describe("the catalog", () => {
  it("gives every service a unique id", () => {
    const ids = SERVICES.map((service) => service.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("gives every resource type a unique id within its service and at least one column", () => {
    for (const service of SERVICES) {
      const ids = service.resources.map((resource) => resource.id);
      expect(new Set(ids).size, service.id).toBe(ids.length);
      for (const resource of service.resources) {
        expect(resource.columns.length, `${service.id}/${resource.id}`).toBeGreaterThan(0);
      }
    }
  });

  it("finds a resource type by its service and resource ids", () => {
    expect(findResource(findService("s3"), "buckets")?.title).toBe("Buckets");
  });

  it("finds no service for an unknown id", () => {
    expect(findService("lambda")).toBeUndefined();
  });

  it("finds no resource type for an unknown id", () => {
    expect(findResource(findService("s3"), "tables")).toBeUndefined();
  });

  it("keys an item by the field its key names", () => {
    expect(findResource(findService("s3"), "buckets")?.key({ Name: "logs" })).toBe("logs");
  });

  it("refuses to key an item that lacks the field", () => {
    expect(() => findResource(findService("s3"), "buckets")?.key({})).toThrow(/Name/);
  });

  it("groups every service under its category", () => {
    const grouped = [...servicesByCategory().values()].flat();
    expect(grouped).toHaveLength(SERVICES.length);
  });
});
