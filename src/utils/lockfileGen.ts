import { makeRequest } from "./httpHelper.ts"
import semver from "semver";
import type { Message } from "./logger.ts";
import { existsSync, mkdirSync, writeFileSync } from "fs";

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


export function makeDirectories(lockfileJson : LockFile) {
    if (!existsSync("packages")) {
        mkdirSync("packages");
    }

    const nonPrimaryDeps : Record<string, {name : string, version : string}> = {};

    for (const [packageName, versions] of Object.entries(lockfileJson.packages)) {
        for (const version of versions) {
            const dirPath = `./${version.location}/${packageName}`;
            if (!existsSync(dirPath)) {
                mkdirSync(dirPath,  { recursive: true });
            }

            //TODO: Stream in the actual package files from the registry

            let hasPrimaryDependency = false;
            let dependencyCount = 0;
            for (const [depName, depInfo] of Object.entries(version.dependencies)) {
                dependencyCount++;
                if (depInfo.primary) {
                    hasPrimaryDependency = true;
                    continue; // Skip primary dependencies
                }

                // Store non-primary dependencies for later processing
                nonPrimaryDeps[dirPath + "/packages/" + depName] = { name : depName, version: depInfo.version };
            }

            if (!hasPrimaryDependency && dependencyCount > 0) {
                mkdirSync(dirPath + "/packages");
            }
        }
    }

    for (const [location, depInfo] of Object.entries(nonPrimaryDeps)) {
        // Optionally, you can create a placeholder file for the dependency
        if (!existsSync(location)) {
            mkdirSync(location);
        }

        const parts = location.split("/");
        const prefix = "script" + (".Parent".repeat(parts.length - 1)); // Remove the ./ and the name
        
        const targetLocation = lockfileJson.packages[depInfo.name]?.find(pkg => pkg.version === depInfo.version)?.location;
        if (!targetLocation) {
            throw new Error(`Target location for ${depInfo.name} @ ${depInfo.version} not found in lockfile.`);
            continue;
        }

        let pathFromRoot = targetLocation.split("/").slice(1).join(`"]["`); // Remove the first part (the packages folder)
        if (pathFromRoot !==  "") {
            pathFromRoot = `["${pathFromRoot}"]`; // Add a dot at the start
        }

        const luaPath = `${prefix}${pathFromRoot}["${depInfo.name}"]`; // Remove the last part (the package name)

        writeFileSync(`${location}/init.lua`, `--Pointer file\nreturn require(${luaPath})`, { encoding: "utf-8",  });
    }
}

export async function lockfileGen(forestJson: ForestJson, msg : Message) : Promise<string> {
    const lockfileContent: LockFile = {
        fileVersion : 1,
        packages : {},
    };


    async function makeDepTree(packageName : string, version : string, location : string) {
        let response : {version : string, dependencies?: Record<string, string>};
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

        
        for (const [depName, depVersion] of Object.entries(response.dependencies || {})) {
            if (!semver.validRange(depVersion)) {
                console.warn(`Skipping invalid version range for dependency ${depName}: ${depVersion}`);
                return;
            }

            let currentInstalledVersions : Array<string> = [];
            if (lockfileContent.packages[depName]) {
                currentInstalledVersions = lockfileContent.packages[depName].map(pkg => pkg.version);
            }

            
            let depExists = semver.maxSatisfying(currentInstalledVersions, depVersion);
            //console.log(depExists, depName, currentInstalledVersions)

            depsDict[depName] = {
                version: depVersion as string,
                primary: depExists == undefined,
            };

            if (depExists == undefined) {
                await makeDepTree(depName, depVersion as string, location + "/" + packageName + "/packages");
            }
        }
    }

    msg.update("Updating workspace dependencies...");
    
    for (const [name, version] of Object.entries(forestJson.dependencies || {})) {
        await makeDepTree(name, version, "packages");
    }

    makeDirectories(lockfileContent);

    return JSON.stringify(lockfileContent, null, 2);
}