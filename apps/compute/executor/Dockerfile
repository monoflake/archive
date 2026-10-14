# syntax=docker/dockerfile:1
# check=skip=InvalidDefaultArgInFrom
# The executor: the driver, a turn's thread and every workflow bundled by scripts/bundle.ts, run by
# the Node the workspace pins and nothing else, since the bundle carries every dependency. Built
# from the repository root by `mise run image`. See spec/architecture/executor.md.
ARG NODE_VERSION

FROM node:${NODE_VERSION}-slim AS build
WORKDIR /src
# The pnpm the repository names, since this Node carries no corepack to pick it.
COPY package.json ./
RUN npm install --global "$(node -p "require('./package.json').packageManager")"
COPY . .
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
	pnpm install --frozen-lockfile --filter "@monoflake/executor..." \
	&& pnpm --filter @monoflake/executor run bundle

FROM node:${NODE_VERSION}-slim
WORKDIR /executor
COPY --from=build /src/apps/compute/executor/dist ./
# The image's own `node` user, by number, so host runs it as that and nothing more.
USER 1000:1000
ENV NODE_ENV=production
CMD ["node", "main.mjs"]
