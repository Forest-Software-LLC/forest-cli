import { makeRequest } from "./httpHelper"
import semver from "semver";
import type { Message } from "./logger";

type PackageDependency = {
    version: string,
    primary: boolean,
}

type Package = {
    version : string,
    resolved : string,
    integrity : string,
    location? : string,
    dependencies : Record<string, PackageDependency>,
}

type LockFile = {
    fileVersion : number,
    packages : Record<string, Array<Package>>,
}

export type ForestJson = {
    name : string,
    version : string,
    platform : string,
    license? : string,
    dependencies : Record<string, string>,
}

export async function lockfileGen(forestJson: ForestJson, msg : Message) : Promise<string> {
    const lockfileContent: LockFile = {
        fileVersion : 1,
        packages : {},
    };


    async function makeDepTree(packageName : string, version : string, location : string) {
        let response;
        try {
            response = await makeRequest(`v1/package/get?packageId=${packageName}&version=${encodeURIComponent(version)}`, {
                method : "GET",
            })
        } catch (error) {
            console.error(`Failed to fetch package information for ${packageName} @ ${version}:`, error);
            return null
        }

        if (!lockfileContent.packages[packageName]) {
            lockfileContent.packages[packageName] = [];
        }

        let depsDict : Record<string, PackageDependency> = {};
        lockfileContent.packages[packageName].push({ 
            version : response.version,
            resolved: `https://registry.forestpm.dev/`,
            integrity: `abc-1234`,
            dependencies : depsDict,
            location
        });

        await Promise.all(
            Object.entries(response.dependencies || {}).map(async ([depName, depVersion]) => {

                if (!semver.validRange(depVersion)) {
                    console.warn(`Skipping invalid version range for dependency ${depName}: ${depVersion}`);
                    return;
                }

                let currentInstalledVersions : Array<string> = [];
                if (lockfileContent.packages[depName]) {
                    currentInstalledVersions = lockfileContent.packages[depName].map(pkg => pkg.version);
                }

                let depExists = semver.maxSatisfying(currentInstalledVersions, depVersion);

                depsDict[depName] = {
                    version: depVersion as string,
                    primary: depExists == undefined,
                };

                if (depExists == undefined) {
                    return makeDepTree(depName, depVersion as string, location + "/" + depName);
                }
            })
        )
    }

    msg.update("Updating workspace dependencies...");
    await Promise.all(
        Object.entries(forestJson.dependencies || {}).map(([name, version]) => {

            return makeDepTree(name, version, "packages");
        })
    )

    return JSON.stringify(lockfileContent, null, 2);
}